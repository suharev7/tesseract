use actix_web::{
    HttpRequest,
    HttpResponse,
    Result as ActixResult,
};
use serde_derive::Serialize;
use structopt::clap::crate_version;
use tesseract_core::Backend;

use crate::app::AppState;

pub fn index_handler<B: Backend>(_req: HttpRequest<AppState<B>>) -> ActixResult<HttpResponse> {
    Ok(HttpResponse::Ok().json(
        Status {
            status: "ok".to_owned(),
            // TODO set this as the Cargo.toml version, after structopt added
            tesseract_version: crate_version!().to_owned(),
        }
    ))
}

#[derive(Debug, Serialize)]
struct Status {
    status: String,
    tesseract_version: String,
}
