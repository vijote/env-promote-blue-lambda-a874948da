use aws_config::BehaviorVersion;
use aws_sdk_cloudfront::Client as CloudFrontClient;
use lambda_http::{run, service_fn, Body, Error, Request, Response};
use serde_json::json;
use std::env;

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Inicializar el SDK de AWS v1
    let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
    let cloudfront_client = CloudFrontClient::new(&config);

    // Iniciar el runtime de la Lambda HTTP
    run(service_fn(|event: Request| {
        promote_staging_handler(event, &cloudfront_client)
    }))
    .await
}

async fn promote_staging_handler(_req: Request, client: &CloudFrontClient) -> Result<Response<Body>, Error> {
    // 1. Parsear los datos de entrada del HTTP Request

    let primary_distribution_id = env::var("PRIMARY_DISTRIBUTION_ID").expect("PRIMARY_DISTRIBUTION_ID variable is not set!!");
    let staging_distribution_id = env::var("STAGING_DISTRIBUTION_ID").expect("STAGING_DISTRIBUTION_ID variable is not set!!");

    // 2. Para actualizar la distribución primaria con el staging modifier, 
    // AWS CloudFront requiere obligatoriamente el ETag de la distribución Primaria (Producción)
    let primary_config_output = match client
        .get_distribution_config()
        .id(&primary_distribution_id)
        .send()
        .await
    {
        Ok(output) => output,
        Err(err) => {
            return Ok(Response::builder()
                .status(500)
                .body(Body::from(json!({ "error": format!("Error obteniendo config primaria: {:?}", err) }).to_string()))?)
        }
    };

    // Extraemos el ETag de la distribución primaria
    let if_match_etag = primary_config_output.e_tag().unwrap().to_string();

    // 3. Ejecutar la promoción atómica
    // Esto copia los Origins de la staging distribution directamente a la producción estándar.
    match client
        .update_distribution_with_staging_config()
        .id(&primary_distribution_id)
        .staging_distribution_id(&staging_distribution_id)
        .if_match(if_match_etag)
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
                        &staging_distribution_id, &primary_distribution_id
                    )
                }).to_string()))?)
        }
        Err(err) => {
            Ok(Response::builder()
                .status(500)
                .body(Body::from(json!({ "error": format!("Error durante la promoción: {:?}", err) }).to_string()))?)
        }
    }
}