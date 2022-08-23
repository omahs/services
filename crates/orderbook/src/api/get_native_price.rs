use anyhow::Result;
use ethcontract::H160;
use shared::{
    api::convert_json_response,
    price_estimation::native::{native_single_estimate, NativePriceEstimating},
};
use std::{convert::Infallible, sync::Arc};
use warp::{Filter, Rejection};

pub fn get(
    native_price_estimator: Arc<dyn NativePriceEstimating>,
) -> impl Filter<Extract = (super::ApiReply,), Error = Rejection> + Clone {
    warp::path!("prices" / H160)
        .and(warp::get())
        .and_then(move |token: H160| {
            let native_price_estimator = native_price_estimator.clone();
            async move {
                let result = native_single_estimate(&*native_price_estimator, &token).await;
                Result::<_, Infallible>::Ok(convert_json_response(result))
            }
        })
}
