use std::env;

use aws_config::BehaviorVersion;
use aws_sdk_cloudfront::Client as CloudFrontClient;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client as DynamoClient;
use lambda_http::{run, service_fn, Body, Error, Request, Response};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct PromoteRequest {
    primary_distribution_id: String,
    staging_distribution_id: String,
    environment_id: String,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Inicializar el SDK de AWS v1
    let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
    let cloudfront_client = CloudFrontClient::new(&config);
    let dynamo_client = DynamoClient::new(&config);

    // Iniciar el runtime de la Lambda HTTP
    run(service_fn(|event: Request| {
        promote_staging_handler(event, &cloudfront_client, &dynamo_client)
    }))
    .await
}

async fn promote_staging_handler(
    req: Request,
    cloudfront_client: &CloudFrontClient,
    dynamo_client: &DynamoClient,
) -> Result<Response<Body>, Error> {
    // 1. Parsear los datos de entrada del HTTP Request
    let body_bytes = req.body().as_ref();
    let table_name = env::var("TABLE_NAME").expect("TABLE_NAME not set!!");
    let payload: PromoteRequest = match serde_json::from_slice(body_bytes) {
        Ok(p) => p,
        Err(_) => {
            return Ok(Response::builder().status(400).body(Body::from(
                json!({ "error": "Invalid JSON body" }).to_string(),
            ))?)
        }
    };

    // 2. Para actualizar la distribución primaria con el staging modifier,
    // AWS CloudFront requiere obligatoriamente el ETag de la distribución Primaria (Producción)
    let primary_config_output = match cloudfront_client
        .get_distribution_config()
        .id(&payload.primary_distribution_id)
        .send()
        .await
    {
        Ok(output) => output,
        Err(err) => {
            return Ok(Response::builder().status(500).body(Body::from(
                json!({ "error": format!("Error obteniendo config primaria: {:?}", err) })
                    .to_string(),
            ))?)
        }
    };

    let staging_config_output = cloudfront_client
        .get_distribution_config()
        .id(&payload.staging_distribution_id)
        .send()
        .await?;

    let primary_etag = primary_config_output.e_tag().expect("primary etag missing");

    let staging_etag = staging_config_output.e_tag().expect("staging etag missing");

    let if_match = format!("{}, {}", primary_etag, staging_etag);

    if let Err(err) = dynamo_client
        .update_item()
        .table_name(&table_name)
        .key(
            "environment",
            AttributeValue::S(payload.environment_id.to_string()),
        )
        .update_expression("SET env_status = :newStatus")
        .expression_attribute_values(":newStatus", AttributeValue::S("blue".to_string()))
        .send()
        .await
    {
        return Ok(Response::builder().status(500).body(Body::from(
            json!({ "error": format!("Error durante la promoción: {:?}", err) }).to_string(),
        ))?);
    };

    if let Err(err) = update_existing_blue(&dynamo_client, &table_name).await {
        return Ok(Response::builder()
            .status(500)
            .body(Body::from(
                json!({ "error": format!("Error updating blue env! {:?}", err) }).to_string(),
            ))
            .expect("Error setting update blue env response body!"));
    }

    // 3. Ejecutar la promoción atómica
    // Esto copia los Origins de la staging distribution directamente a la producción estándar.
    match cloudfront_client
        .update_distribution_with_staging_config()
        .id(&payload.primary_distribution_id)
        .staging_distribution_id(&payload.staging_distribution_id)
        .if_match(if_match)
        .send()
        .await
    {
        Ok(_) => {
            // Promoción exitosa
            Ok(Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "status": "Promoted",
                    "message": format!(
                        "La distribución de staging {} ha sido promovida exitosamente a la producción estándar {}.",
                        payload.staging_distribution_id, payload.primary_distribution_id
                    )
                }).to_string()))?)
        }
        Err(err) => Ok(Response::builder().status(500).body(Body::from(
            json!({ "error": format!("Error durante la promoción: {:?}", err) }).to_string(),
        ))?),
    }
}

async fn update_existing_blue(
    dynamo_client: &DynamoClient,
    table_name: &str,
) -> Result<(), String> {
    let results = dynamo_client
        .scan()
        .table_name(table_name)
        .filter_expression("env_status = :status_val")
        .expression_attribute_values(":status_val", AttributeValue::S("blue".to_string()))
        // Opcional: Proyecta solo los atributos que necesitas para ahorrar ancho de banda
        .projection_expression("environment")
        .send()
        .await
        .expect("Error finding blue environments!");

    let blue_env = results
        .items
        .as_ref()
        .and_then(|items| items.first())
        .and_then(|item| item.get("environment"))
        .and_then(|attr| match attr {
            AttributeValue::S(s) => Some(s.clone()),
            _ => None,
        });

    if let None = blue_env {
        return Ok(());
    }

    if let Err(err) = dynamo_client
        .update_item()
        .table_name(table_name)
        .key(
            "environment",
            AttributeValue::S(blue_env.expect("Error unwraping blue env id!").to_string()),
        )
        .update_expression("SET env_status = :newStatus")
        .expression_attribute_values(":newStatus", AttributeValue::S("unused".to_string()))
        .send()
        .await
    {
        return Err(format!("Error updating blue environment! {}", err).to_string());
    };

    return Ok(());
}
