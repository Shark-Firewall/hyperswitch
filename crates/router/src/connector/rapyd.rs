pub mod transformers;

use base64::Engine;
use common_utils::{
    date_time,
    ext_traits::{Encode, StringExt},
    request::RequestContent,
    types::{AmountConvertor, MinorUnit, MinorUnitForConnector},
};
use diesel_models::enums;
use error_stack::{Report, ResultExt};
use masking::{ExposeInterface, PeekInterface, Secret};
use rand::distributions::{Alphanumeric, DistString};
use ring::hmac;
use transformers as rapyd;

use super::utils as connector_utils;
use crate::{
    configs::settings,
    connector::utils::convert_amount,
    consts,
    core::errors::{self, CustomResult},
    events::connector_api_logs::ConnectorEvent,
    headers, logger,
    services::{
        self,
        request::{self, Mask},
        ConnectorValidation,
    },
    types::{
        self,
        api::{self, ConnectorCommon},
        ErrorResponse,
    },
    utils::{self, crypto, ByteSliceExt, BytesExt},
};

#[derive(Clone)]
pub struct Rapyd {
    amount_converter: &'static (dyn AmountConvertor<Output = MinorUnit> + Sync),
}
impl Rapyd {
    pub fn new() -> &'static Self {
        &Self {
            amount_converter: &MinorUnitForConnector,
        }
    }
}
impl Rapyd {
    pub fn generate_signature(
        &self,
        auth: &rapyd::RapydAuthType,
        http_method: &str,
        url_path: &str,
        body: &str,
        timestamp: &i64,
        salt: &str,
    ) -> CustomResult<String, errors::ConnectorError> {
        let rapyd::RapydAuthType {
            access_key,
            secret_key,
        } = auth;
        let to_sign = format!(
            "{http_method}{url_path}{salt}{timestamp}{}{}{body}",
            access_key.peek(),
            secret_key.peek()
        );
        let key = hmac::Key::new(hmac::HMAC_SHA256, secret_key.peek().as_bytes());
        let tag = hmac::sign(&key, to_sign.as_bytes());
        let hmac_sign = hex::encode(tag);
        let signature_value = consts::BASE64_ENGINE_URL_SAFE.encode(hmac_sign);
        Ok(signature_value)
    }
}

impl ConnectorCommon for Rapyd {
    fn id(&self) -> &'static str {
        "rapyd"
    }

    fn get_currency_unit(&self) -> api::CurrencyUnit {
        api::CurrencyUnit::Minor
    }

    fn common_get_content_type(&self) -> &'static str {
        "application/json"
    }

    fn base_url<'a>(&self, connectors: &'a settings::Connectors) -> &'a str {
        connectors.rapyd.base_url.as_ref()
    }

    fn get_auth_header(
        &self,
        _auth_type: &types::ConnectorAuthType,
    ) -> CustomResult<Vec<(String, request::Maskable<String>)>, errors::ConnectorError> {
        Ok(vec![])
    }

    fn build_error_response(
        &self,
        res: types::Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        let response: Result<
            rapyd::RapydPaymentsResponse,
            Report<common_utils::errors::ParsingError>,
        > = res.response.parse_struct("Rapyd ErrorResponse");

        match response {
            Ok(response_data) => {
                event_builder.map(|i| i.set_error_response_body(&response_data));
                router_env::logger::info!(connector_response=?response_data);
                Ok(ErrorResponse {
                    status_code: res.status_code,
                    code: response_data.status.error_code,
                    message: response_data.status.status.unwrap_or_default(),
                    reason: response_data.status.message,
                    attempt_status: None,
                    connector_transaction_id: None,
                })
            }
            Err(error_msg) => {
                event_builder.map(|event| event.set_error(serde_json::json!({"error": res.response.escape_ascii().to_string(), "status_code": res.status_code})));
                logger::error!(deserialization_error =? error_msg);
                utils::handle_json_response_deserialization_failure(res, "rapyd")
            }
        }
    }
}

impl ConnectorValidation for Rapyd {
    fn validate_capture_method(
        &self,
        capture_method: Option<enums::CaptureMethod>,
        _pmt: Option<enums::PaymentMethodType>,
    ) -> CustomResult<(), errors::ConnectorError> {
        let capture_method = capture_method.unwrap_or_default();
        match capture_method {
            enums::CaptureMethod::Automatic | enums::CaptureMethod::Manual => Ok(()),
            enums::CaptureMethod::ManualMultiple | enums::CaptureMethod::Scheduled => Err(
                connector_utils::construct_not_supported_error_report(capture_method, self.id()),
            ),
        }
    }
}

impl api::ConnectorAccessToken for Rapyd {}

impl api::PaymentToken for Rapyd {}

impl
    services::ConnectorIntegration<
        api::PaymentMethodToken,
        types::PaymentMethodTokenizationData,
        types::PaymentsResponseData,
    > for Rapyd
{
    // Not Implemented (R)
}

impl
    services::ConnectorIntegration<
        api::AccessTokenAuth,
        types::AccessTokenRequestData,
        types::AccessToken,
    > for Rapyd
{
}

impl api::PaymentAuthorize for Rapyd {}

impl
    services::ConnectorIntegration<
        api::Authorize,
        types::PaymentsAuthorizeData,
        types::PaymentsResponseData,
    > for Rapyd
{
    fn get_headers(
        &self,
        _req: &types::PaymentsAuthorizeRouterData,
        _connectors: &settings::Connectors,
    ) -> CustomResult<Vec<(String, request::Maskable<String>)>, errors::ConnectorError> {
        Ok(vec![(
            headers::CONTENT_TYPE.to_string(),
            types::PaymentsAuthorizeType::get_content_type(self)
                .to_string()
                .into(),
        )])
    }

    fn get_content_type(&self) -> &'static str {
        self.common_get_content_type()
    }

    fn get_url(
        &self,
        _req: &types::PaymentsAuthorizeRouterData,
        connectors: &settings::Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!("{}/v1/payments", self.base_url(connectors)))
    }

    fn get_request_body(
        &self,
        req: &types::PaymentsAuthorizeRouterData,
        _connectors: &settings::Connectors,
    ) -> CustomResult<RequestContent, errors::ConnectorError> {
        let amount = convert_amount(
            self.amount_converter,
            req.request.minor_amount,
            req.request.currency,
        )?;
        let connector_router_data = rapyd::RapydRouterData::from((amount, req));
        let connector_req = rapyd::RapydPaymentsRequest::try_from(&connector_router_data)?;
        Ok(RequestContent::Json(Box::new(connector_req)))
    }

    fn build_request(
        &self,
        req: &types::RouterData<
            api::Authorize,
            types::PaymentsAuthorizeData,
            types::PaymentsResponseData,
        >,
        connectors: &settings::Connectors,
    ) -> CustomResult<Option<services::Request>, errors::ConnectorError> {
        let timestamp = date_time::now_unix_timestamp();
        let salt = Alphanumeric.sample_string(&mut rand::thread_rng(), 12);

        let auth: rapyd::RapydAuthType = rapyd::RapydAuthType::try_from(&req.connector_auth_type)?;
        let body = types::PaymentsAuthorizeType::get_request_body(self, req, connectors)?;
        let req_body = body.get_inner_value().expose();
        let signature =
            self.generate_signature(&auth, "post", "/v1/payments", &req_body, &timestamp, &salt)?;
        let headers = vec![
            ("access_key".to_string(), auth.access_key.into_masked()),
            ("salt".to_string(), salt.into_masked()),
            ("timestamp".to_string(), timestamp.to_string().into()),
            ("signature".to_string(), signature.into_masked()),
        ];
        let request = services::RequestBuilder::new()
            .method(services::Method::Post)
            .url(&types::PaymentsAuthorizeType::get_url(
                self, req, connectors,
            )?)
            .attach_default_headers()
            .headers(types::PaymentsAuthorizeType::get_headers(
                self, req, connectors,
            )?)
            .headers(headers)
            .set_body(types::PaymentsAuthorizeType::get_request_body(
                self, req, connectors,
            )?)
            .build();
        Ok(Some(request))
    }

    fn handle_response(
        &self,
        data: &types::PaymentsAuthorizeRouterData,
        event_builder: Option<&mut ConnectorEvent>,
        res: types::Response,
    ) -> CustomResult<types::PaymentsAuthorizeRouterData, errors::ConnectorError> {
        let response: rapyd::RapydPaymentsResponse = res
            .response
            .parse_struct("Rapyd PaymentResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;
        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);
        types::RouterData::try_from(types::ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
        .change_context(errors::ConnectorError::ResponseHandlingFailed)
    }

    fn get_error_response(
        &self,
        res: types::Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        self.build_error_response(res, event_builder)
    }
}

impl api::Payment for Rapyd {}

impl api::MandateSetup for Rapyd {}
impl
    services::ConnectorIntegration<
        api::SetupMandate,
        types::SetupMandateRequestData,
        types::PaymentsResponseData,
    > for Rapyd
{
    fn build_request(
        &self,
        _req: &types::RouterData<
            api::SetupMandate,
            types::SetupMandateRequestData,
            types::PaymentsResponseData,
        >,
        _connectors: &settings::Connectors,
    ) -> CustomResult<Option<services::Request>, errors::ConnectorError> {
        Err(
            errors::ConnectorError::NotImplemented("Setup Mandate flow for Rapyd".to_string())
                .into(),
        )
    }
}

impl api::PaymentVoid for Rapyd {}

impl
    services::ConnectorIntegration<
        api::Void,
        types::PaymentsCancelData,
        types::PaymentsResponseData,
    > for Rapyd
{
    fn get_headers(
        &self,
        _req: &types::PaymentsCancelRouterData,
        _connectors: &settings::Connectors,
    ) -> CustomResult<Vec<(String, request::Maskable<String>)>, errors::ConnectorError> {
        Ok(vec![(
            headers::CONTENT_TYPE.to_string(),
            types::PaymentsVoidType::get_content_type(self)
                .to_string()
                .into(),
        )])
    }

    fn get_content_type(&self) -> &'static str {
        self.common_get_content_type()
    }

    fn get_url(
        &self,
        req: &types::PaymentsCancelRouterData,
        connectors: &settings::Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!(
            "{}/v1/payments/{}",
            self.base_url(connectors),
            req.request.connector_transaction_id
        ))
    }

    fn build_request(
        &self,
        req: &types::PaymentsCancelRouterData,
        connectors: &settings::Connectors,
    ) -> CustomResult<Option<services::Request>, errors::ConnectorError> {
        let timestamp = date_time::now_unix_timestamp();
        let salt = Alphanumeric.sample_string(&mut rand::thread_rng(), 12);

        let auth: rapyd::RapydAuthType = rapyd::RapydAuthType::try_from(&req.connector_auth_type)?;
        let url_path = format!("/v1/payments/{}", req.request.connector_transaction_id);
        let signature =
            self.generate_signature(&auth, "delete", &url_path, "", &timestamp, &salt)?;

        let headers = vec![
            ("access_key".to_string(), auth.access_key.into_masked()),
            ("salt".to_string(), salt.into_masked()),
            ("timestamp".to_string(), timestamp.to_string().into()),
            ("signature".to_string(), signature.into_masked()),
        ];
        let request = services::RequestBuilder::new()
            .method(services::Method::Delete)
            .url(&types::PaymentsVoidType::get_url(self, req, connectors)?)
            .attach_default_headers()
            .headers(types::PaymentsVoidType::get_headers(self, req, connectors)?)
            .headers(headers)
            .build();
        Ok(Some(request))
    }

    fn handle_response(
        &self,
        data: &types::PaymentsCancelRouterData,
        event_builder: Option<&mut ConnectorEvent>,
        res: types::Response,
    ) -> CustomResult<types::PaymentsCancelRouterData, errors::ConnectorError> {
        let response: rapyd::RapydPaymentsResponse = res
            .response
            .parse_struct("Rapyd PaymentResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;
        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);
        types::RouterData::try_from(types::ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
        .change_context(errors::ConnectorError::ResponseHandlingFailed)
    }

    fn get_error_response(
        &self,
        res: types::Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        self.build_error_response(res, event_builder)
    }
}

impl api::PaymentSync for Rapyd {}
impl
    services::ConnectorIntegration<api::PSync, types::PaymentsSyncData, types::PaymentsResponseData>
    for Rapyd
{
    fn get_headers(
        &self,
        _req: &types::PaymentsSyncRouterData,
        _connectors: &settings::Connectors,
    ) -> CustomResult<Vec<(String, request::Maskable<String>)>, errors::ConnectorError> {
        Ok(vec![(
            headers::CONTENT_TYPE.to_string(),
            types::PaymentsSyncType::get_content_type(self)
                .to_string()
                .into(),
        )])
    }

    fn get_content_type(&self) -> &'static str {
        self.common_get_content_type()
    }

    fn get_url(
        &self,
        req: &types::PaymentsSyncRouterData,
        connectors: &settings::Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        let id = req.request.connector_transaction_id.clone();
        Ok(format!(
            "{}/v1/payments/{}",
            self.base_url(connectors),
            id.get_connector_transaction_id()
                .change_context(errors::ConnectorError::MissingConnectorTransactionID)?
        ))
    }

    fn build_request(
        &self,
        req: &types::PaymentsSyncRouterData,
        connectors: &settings::Connectors,
    ) -> CustomResult<Option<services::Request>, errors::ConnectorError> {
        let timestamp = date_time::now_unix_timestamp();
        let salt = Alphanumeric.sample_string(&mut rand::thread_rng(), 12);

        let auth: rapyd::RapydAuthType = rapyd::RapydAuthType::try_from(&req.connector_auth_type)?;
        let response_id = req.request.connector_transaction_id.clone();
        let url_path = format!(
            "/v1/payments/{}",
            response_id
                .get_connector_transaction_id()
                .change_context(errors::ConnectorError::MissingConnectorTransactionID)?
        );
        let signature = self.generate_signature(&auth, "get", &url_path, "", &timestamp, &salt)?;

        let headers = vec![
            ("access_key".to_string(), auth.access_key.into_masked()),
            ("salt".to_string(), salt.into_masked()),
            ("timestamp".to_string(), timestamp.to_string().into()),
            ("signature".to_string(), signature.into_masked()),
        ];
        let request = services::RequestBuilder::new()
            .method(services::Method::Get)
            .url(&types::PaymentsSyncType::get_url(self, req, connectors)?)
            .attach_default_headers()
            .headers(types::PaymentsSyncType::get_headers(self, req, connectors)?)
            .headers(headers)
            .build();
        Ok(Some(request))
    }

    fn get_error_response(
        &self,
        res: types::Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        self.build_error_response(res, event_builder)
    }

    fn handle_response(
        &self,
        data: &types::PaymentsSyncRouterData,
        event_builder: Option<&mut ConnectorEvent>,
        res: types::Response,
    ) -> CustomResult<types::PaymentsSyncRouterData, errors::ConnectorError> {
        let response: rapyd::RapydPaymentsResponse = res
            .response
            .parse_struct("Rapyd PaymentResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;
        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);
        types::RouterData::try_from(types::ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
        .change_context(errors::ConnectorError::ResponseHandlingFailed)
    }
}

impl api::PaymentCapture for Rapyd {}
impl
    services::ConnectorIntegration<
        api::Capture,
        types::PaymentsCaptureData,
        types::PaymentsResponseData,
    > for Rapyd
{
    fn get_headers(
        &self,
        _req: &types::PaymentsCaptureRouterData,
        _connectors: &settings::Connectors,
    ) -> CustomResult<Vec<(String, request::Maskable<String>)>, errors::ConnectorError> {
        Ok(vec![(
            headers::CONTENT_TYPE.to_string(),
            types::PaymentsCaptureType::get_content_type(self)
                .to_string()
                .into(),
        )])
    }

    fn get_content_type(&self) -> &'static str {
        self.common_get_content_type()
    }

    fn get_request_body(
        &self,
        req: &types::PaymentsCaptureRouterData,
        _connectors: &settings::Connectors,
    ) -> CustomResult<RequestContent, errors::ConnectorError> {
        let amount = convert_amount(
            self.amount_converter,
            req.request.minor_amount_to_capture,
            req.request.currency,
        )?;
        let connector_router_data = rapyd::RapydRouterData::from((amount, req));
        let connector_req = rapyd::CaptureRequest::try_from(&connector_router_data)?;
        Ok(RequestContent::Json(Box::new(connector_req)))
    }

    fn build_request(
        &self,
        req: &types::PaymentsCaptureRouterData,
        connectors: &settings::Connectors,
    ) -> CustomResult<Option<services::Request>, errors::ConnectorError> {
        let timestamp = date_time::now_unix_timestamp();
        let salt = Alphanumeric.sample_string(&mut rand::thread_rng(), 12);

        let auth: rapyd::RapydAuthType = rapyd::RapydAuthType::try_from(&req.connector_auth_type)?;
        let url_path = format!(
            "/v1/payments/{}/capture",
            req.request.connector_transaction_id
        );
        let body = types::PaymentsCaptureType::get_request_body(self, req, connectors)?;
        let req_body = body.get_inner_value().expose();
        let signature =
            self.generate_signature(&auth, "post", &url_path, &req_body, &timestamp, &salt)?;
        let headers = vec![
            ("access_key".to_string(), auth.access_key.into_masked()),
            ("salt".to_string(), salt.into_masked()),
            ("timestamp".to_string(), timestamp.to_string().into()),
            ("signature".to_string(), signature.into_masked()),
        ];
        let request = services::RequestBuilder::new()
            .method(services::Method::Post)
            .url(&types::PaymentsCaptureType::get_url(self, req, connectors)?)
            .attach_default_headers()
            .headers(types::PaymentsCaptureType::get_headers(
                self, req, connectors,
            )?)
            .headers(headers)
            .set_body(types::PaymentsCaptureType::get_request_body(
                self, req, connectors,
            )?)
            .build();
        Ok(Some(request))
    }

    fn handle_response(
        &self,
        data: &types::PaymentsCaptureRouterData,
        event_builder: Option<&mut ConnectorEvent>,
        res: types::Response,
    ) -> CustomResult<types::PaymentsCaptureRouterData, errors::ConnectorError> {
        let response: rapyd::RapydPaymentsResponse = res
            .response
            .parse_struct("RapydPaymentResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;

        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);

        types::RouterData::try_from(types::ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
        .change_context(errors::ConnectorError::ResponseHandlingFailed)
    }

    fn get_url(
        &self,
        req: &types::PaymentsCaptureRouterData,
        connectors: &settings::Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!(
            "{}/v1/payments/{}/capture",
            self.base_url(connectors),
            req.request.connector_transaction_id
        ))
    }

    fn get_error_response(
        &self,
        res: types::Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        self.build_error_response(res, event_builder)
    }
}

impl api::PaymentSession for Rapyd {}

impl
    services::ConnectorIntegration<
        api::Session,
        types::PaymentsSessionData,
        types::PaymentsResponseData,
    > for Rapyd
{
    //TODO: implement sessions flow
}

impl api::Refund for Rapyd {}
impl api::RefundExecute for Rapyd {}
impl api::RefundSync for Rapyd {}

impl services::ConnectorIntegration<api::Execute, types::RefundsData, types::RefundsResponseData>
    for Rapyd
{
    fn get_headers(
        &self,
        _req: &types::RefundsRouterData<api::Execute>,
        _connectors: &settings::Connectors,
    ) -> CustomResult<Vec<(String, request::Maskable<String>)>, errors::ConnectorError> {
        Ok(vec![(
            headers::CONTENT_TYPE.to_string(),
            types::RefundExecuteType::get_content_type(self)
                .to_string()
                .into(),
        )])
    }

    fn get_content_type(&self) -> &'static str {
        ConnectorCommon::common_get_content_type(self)
    }

    fn get_url(
        &self,
        _req: &types::RefundsRouterData<api::Execute>,
        connectors: &settings::Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!("{}/v1/refunds", self.base_url(connectors)))
    }

    fn get_request_body(
        &self,
        req: &types::RefundsRouterData<api::Execute>,
        _connectors: &settings::Connectors,
    ) -> CustomResult<RequestContent, errors::ConnectorError> {
        let amount = convert_amount(
            self.amount_converter,
            req.request.minor_refund_amount,
            req.request.currency,
        )?;
        let connector_router_data = rapyd::RapydRouterData::from((amount, req));
        let connector_req = rapyd::RapydRefundRequest::try_from(&connector_router_data)?;

        Ok(RequestContent::Json(Box::new(connector_req)))
    }

    fn build_request(
        &self,
        req: &types::RefundsRouterData<api::Execute>,
        connectors: &settings::Connectors,
    ) -> CustomResult<Option<services::Request>, errors::ConnectorError> {
        let timestamp = date_time::now_unix_timestamp();
        let salt = Alphanumeric.sample_string(&mut rand::thread_rng(), 12);

        let body = types::RefundExecuteType::get_request_body(self, req, connectors)?;
        let req_body = body.get_inner_value().expose();
        let auth: rapyd::RapydAuthType = rapyd::RapydAuthType::try_from(&req.connector_auth_type)?;
        let signature =
            self.generate_signature(&auth, "post", "/v1/refunds", &req_body, &timestamp, &salt)?;
        let headers = vec![
            ("access_key".to_string(), auth.access_key.into_masked()),
            ("salt".to_string(), salt.into_masked()),
            ("timestamp".to_string(), timestamp.to_string().into()),
            ("signature".to_string(), signature.into_masked()),
        ];
        let request = services::RequestBuilder::new()
            .method(services::Method::Post)
            .url(&types::RefundExecuteType::get_url(self, req, connectors)?)
            .attach_default_headers()
            .headers(headers)
            .set_body(types::RefundExecuteType::get_request_body(
                self, req, connectors,
            )?)
            .build();
        Ok(Some(request))
    }

    fn handle_response(
        &self,
        data: &types::RefundsRouterData<api::Execute>,
        event_builder: Option<&mut ConnectorEvent>,
        res: types::Response,
    ) -> CustomResult<types::RefundsRouterData<api::Execute>, errors::ConnectorError> {
        let response: rapyd::RefundResponse = res
            .response
            .parse_struct("rapyd RefundResponse")
            .change_context(errors::ConnectorError::RequestEncodingFailed)?;
        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);
        types::RouterData::try_from(types::ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
        .change_context(errors::ConnectorError::ResponseHandlingFailed)
    }

    fn get_error_response(
        &self,
        res: types::Response,
        event_builder: Option<&mut ConnectorEvent>,
    ) -> CustomResult<ErrorResponse, errors::ConnectorError> {
        self.build_error_response(res, event_builder)
    }
}

impl services::ConnectorIntegration<api::RSync, types::RefundsData, types::RefundsResponseData>
    for Rapyd
{
    // default implementation of build_request method will be executed
    fn handle_response(
        &self,
        data: &types::RefundSyncRouterData,
        event_builder: Option<&mut ConnectorEvent>,
        res: types::Response,
    ) -> CustomResult<types::RefundSyncRouterData, errors::ConnectorError> {
        let response: rapyd::RefundResponse = res
            .response
            .parse_struct("rapyd RefundResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;
        event_builder.map(|i| i.set_response_body(&response));
        router_env::logger::info!(connector_response=?response);
        types::RouterData::try_from(types::ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
        .change_context(errors::ConnectorError::ResponseHandlingFailed)
    }
}

#[async_trait::async_trait]
impl api::IncomingWebhook for Rapyd {
    fn get_webhook_source_verification_algorithm(
        &self,
        _request: &api::IncomingWebhookRequestDetails<'_>,
    ) -> CustomResult<Box<dyn crypto::VerifySignature + Send>, errors::ConnectorError> {
        Ok(Box::new(crypto::HmacSha256))
    }

    fn get_webhook_source_verification_signature(
        &self,
        request: &api::IncomingWebhookRequestDetails<'_>,
        _connector_webhook_secrets: &api_models::webhooks::ConnectorWebhookSecrets,
    ) -> CustomResult<Vec<u8>, errors::ConnectorError> {
        let base64_signature = connector_utils::get_header_key_value("signature", request.headers)?;
        let signature = consts::BASE64_ENGINE_URL_SAFE
            .decode(base64_signature.as_bytes())
            .change_context(errors::ConnectorError::WebhookSourceVerificationFailed)?;
        Ok(signature)
    }

    fn get_webhook_source_verification_message(
        &self,
        request: &api::IncomingWebhookRequestDetails<'_>,
        merchant_id: &common_utils::id_type::MerchantId,
        connector_webhook_secrets: &api_models::webhooks::ConnectorWebhookSecrets,
    ) -> CustomResult<Vec<u8>, errors::ConnectorError> {
        let host = connector_utils::get_header_key_value("host", request.headers)?;
        let connector = self.id();
        let url_path = format!(
            "https://{host}/webhooks/{}/{connector}",
            merchant_id.get_string_repr()
        );
        let salt = connector_utils::get_header_key_value("salt", request.headers)?;
        let timestamp = connector_utils::get_header_key_value("timestamp", request.headers)?;
        let stringify_auth = String::from_utf8(connector_webhook_secrets.secret.to_vec())
            .change_context(errors::ConnectorError::WebhookSourceVerificationFailed)
            .attach_printable("Could not convert secret to UTF-8")?;
        let auth: transformers::RapydAuthType = stringify_auth
            .parse_struct("RapydAuthType")
            .change_context(errors::ConnectorError::WebhookSourceVerificationFailed)?;
        let access_key = auth.access_key;
        let secret_key = auth.secret_key;
        let body_string = String::from_utf8(request.body.to_vec())
            .change_context(errors::ConnectorError::WebhookSourceVerificationFailed)
            .attach_printable("Could not convert body to UTF-8")?;
        let to_sign = format!(
            "{url_path}{salt}{timestamp}{}{}{body_string}",
            access_key.peek(),
            secret_key.peek()
        );

        Ok(to_sign.into_bytes())
    }

    async fn verify_webhook_source(
        &self,
        request: &api::IncomingWebhookRequestDetails<'_>,
        merchant_id: &common_utils::id_type::MerchantId,
        connector_webhook_details: Option<common_utils::pii::SecretSerdeValue>,
        _connector_account_details: crypto::Encryptable<Secret<serde_json::Value>>,
        connector_label: &str,
    ) -> CustomResult<bool, errors::ConnectorError> {
        let connector_webhook_secrets = self
            .get_webhook_source_verification_merchant_secret(
                merchant_id,
                connector_label,
                connector_webhook_details,
            )
            .await
            .change_context(errors::ConnectorError::WebhookSourceVerificationFailed)?;
        let signature = self
            .get_webhook_source_verification_signature(request, &connector_webhook_secrets)
            .change_context(errors::ConnectorError::WebhookSourceVerificationFailed)?;
        let message = self
            .get_webhook_source_verification_message(
                request,
                merchant_id,
                &connector_webhook_secrets,
            )
            .change_context(errors::ConnectorError::WebhookSourceVerificationFailed)?;

        let stringify_auth = String::from_utf8(connector_webhook_secrets.secret.to_vec())
            .change_context(errors::ConnectorError::WebhookSourceVerificationFailed)
            .attach_printable("Could not convert secret to UTF-8")?;
        let auth: transformers::RapydAuthType = stringify_auth
            .parse_struct("RapydAuthType")
            .change_context(errors::ConnectorError::WebhookSourceVerificationFailed)?;
        let secret_key = auth.secret_key;
        let key = hmac::Key::new(hmac::HMAC_SHA256, secret_key.peek().as_bytes());
        let tag = hmac::sign(&key, &message);
        let hmac_sign = hex::encode(tag);
        Ok(hmac_sign.as_bytes().eq(&signature))
    }

    fn get_webhook_object_reference_id(
        &self,
        request: &api::IncomingWebhookRequestDetails<'_>,
    ) -> CustomResult<api_models::webhooks::ObjectReferenceId, errors::ConnectorError> {
        let webhook: transformers::RapydIncomingWebhook = request
            .body
            .parse_struct("RapydIncomingWebhook")
            .change_context(errors::ConnectorError::WebhookEventTypeNotFound)?;

        Ok(match webhook.data {
            transformers::WebhookData::Payment(payment_data) => {
                api_models::webhooks::ObjectReferenceId::PaymentId(
                    api_models::payments::PaymentIdType::ConnectorTransactionId(payment_data.id),
                )
            }
            transformers::WebhookData::Refund(refund_data) => {
                api_models::webhooks::ObjectReferenceId::RefundId(
                    api_models::webhooks::RefundIdType::ConnectorRefundId(refund_data.id),
                )
            }
            transformers::WebhookData::Dispute(dispute_data) => {
                api_models::webhooks::ObjectReferenceId::PaymentId(
                    api_models::payments::PaymentIdType::ConnectorTransactionId(
                        dispute_data.original_transaction_id,
                    ),
                )
            }
        })
    }

    fn get_webhook_event_type(
        &self,
        request: &api::IncomingWebhookRequestDetails<'_>,
    ) -> CustomResult<api::IncomingWebhookEvent, errors::ConnectorError> {
        let webhook: transformers::RapydIncomingWebhook = request
            .body
            .parse_struct("RapydIncomingWebhook")
            .change_context(errors::ConnectorError::WebhookEventTypeNotFound)?;
        Ok(match webhook.webhook_type {
            rapyd::RapydWebhookObjectEventType::PaymentCompleted
            | rapyd::RapydWebhookObjectEventType::PaymentCaptured => {
                api::IncomingWebhookEvent::PaymentIntentSuccess
            }
            rapyd::RapydWebhookObjectEventType::PaymentFailed => {
                api::IncomingWebhookEvent::PaymentIntentFailure
            }
            rapyd::RapydWebhookObjectEventType::PaymentRefundFailed
            | rapyd::RapydWebhookObjectEventType::PaymentRefundRejected => {
                api::IncomingWebhookEvent::RefundFailure
            }
            rapyd::RapydWebhookObjectEventType::RefundCompleted => {
                api::IncomingWebhookEvent::RefundSuccess
            }
            rapyd::RapydWebhookObjectEventType::PaymentDisputeCreated => {
                api::IncomingWebhookEvent::DisputeOpened
            }
            rapyd::RapydWebhookObjectEventType::Unknown => {
                api::IncomingWebhookEvent::EventNotSupported
            }
            rapyd::RapydWebhookObjectEventType::PaymentDisputeUpdated => match webhook.data {
                rapyd::WebhookData::Dispute(data) => api::IncomingWebhookEvent::from(data.status),
                _ => api::IncomingWebhookEvent::EventNotSupported,
            },
        })
    }

    fn get_webhook_resource_object(
        &self,
        request: &api::IncomingWebhookRequestDetails<'_>,
    ) -> CustomResult<Box<dyn masking::ErasedMaskSerialize>, errors::ConnectorError> {
        let webhook: transformers::RapydIncomingWebhook = request
            .body
            .parse_struct("RapydIncomingWebhook")
            .change_context(errors::ConnectorError::WebhookEventTypeNotFound)?;
        let res_json = match webhook.data {
            transformers::WebhookData::Payment(payment_data) => {
                let rapyd_response: transformers::RapydPaymentsResponse = payment_data.into();

                rapyd_response
                    .encode_to_value()
                    .change_context(errors::ConnectorError::WebhookResourceObjectNotFound)?
            }
            transformers::WebhookData::Refund(refund_data) => refund_data
                .encode_to_value()
                .change_context(errors::ConnectorError::WebhookResourceObjectNotFound)?,
            transformers::WebhookData::Dispute(dispute_data) => dispute_data
                .encode_to_value()
                .change_context(errors::ConnectorError::WebhookResourceObjectNotFound)?,
        };
        Ok(Box::new(res_json))
    }

    fn get_dispute_details(
        &self,
        request: &api::IncomingWebhookRequestDetails<'_>,
    ) -> CustomResult<api::disputes::DisputePayload, errors::ConnectorError> {
        let webhook: transformers::RapydIncomingWebhook = request
            .body
            .parse_struct("RapydIncomingWebhook")
            .change_context(errors::ConnectorError::WebhookEventTypeNotFound)?;
        let webhook_dispute_data = match webhook.data {
            transformers::WebhookData::Dispute(dispute_data) => Ok(dispute_data),
            _ => Err(errors::ConnectorError::WebhookBodyDecodingFailed),
        }?;
        Ok(api::disputes::DisputePayload {
            amount: webhook_dispute_data.amount.to_string(),
            currency: webhook_dispute_data.currency.to_string(),
            dispute_stage: api_models::enums::DisputeStage::Dispute,
            connector_dispute_id: webhook_dispute_data.token,
            connector_reason: Some(webhook_dispute_data.dispute_reason_description),
            connector_reason_code: None,
            challenge_required_by: webhook_dispute_data.due_date,
            connector_status: webhook_dispute_data.status.to_string(),
            created_at: webhook_dispute_data.created_at,
            updated_at: webhook_dispute_data.updated_at,
        })
    }
}
