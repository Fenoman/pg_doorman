use crate::client::core::Client;
use crate::config::config_arc;
use crate::errors::Error;
use crate::messages::error_response_timeout;

impl<S, T> Client<S, T>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    /// Helper to send error response and return the error
    pub(crate) async fn send_error_response(
        &mut self,
        message: &str,
        code: &str,
        err: Error,
    ) -> Result<(), Error> {
        let write_timeout = config_arc().general.proxy_copy_data_timeout.as_std();
        if let Err(write_err) =
            error_response_timeout(&mut self.write, message, code, write_timeout).await
        {
            log::warn!(
                "[{}@{} #c{}] failed to send client ErrorResponse: {write_err}",
                self.username,
                self.pool_name,
                self.connection_id,
            );
        }
        Err(err)
    }

    pub(crate) async fn process_error(&mut self, err: Error) -> Result<(), Error> {
        match err {
            Error::MaxMessageSize => {
                self.send_error_response(
                    "Message exceeds maximum allowed size. Please reduce the size of your query or data.",
                    "53200",
                    err,
                ).await
            }
            Error::CurrentMemoryUsage => {
                self.send_error_response(
                    "Server is temporarily out of memory. Please try again later or reduce the size of your query.",
                    "53200",
                    err,
                ).await
            }
            // sanitized - internal Display of
            // SocketError / ConnectError / ConnectResourceExhausted /
            // ServerUnavailableError carries backend addresses (Patroni
            // discovery URLs, internal host:port, fd numbers from EMFILE
            // strings, raw io::Error text). Previously leaked to
            // unauthenticated clients via ErrorResponse. Log full detail
            // server-side; client sees generic category + SQLSTATE.
            Error::SocketError(ref msg) => {
                log::warn!(
                    "[{}@{} #c{}] socket error suppressed from client: {msg}",
                    self.username, self.pool_name, self.connection_id,
                );
                self.send_error_response(
                    "Network connection error. Please check your network connection.",
                    "08006",
                    err,
                ).await
            }
            Error::ConnectError(ref msg) => {
                log::warn!(
                    "[{}@{} #c{}] connect error suppressed from client: {msg}",
                    self.username, self.pool_name, self.connection_id,
                );
                self.send_error_response(
                    "Network connection error. Please check your network connection.",
                    "08006",
                    err,
                ).await
            }
            Error::ConnectResourceExhausted(ref msg) => {
                log::warn!(
                    "[{}@{} #c{}] resource exhausted (suppressed from client): {msg}",
                    self.username, self.pool_name, self.connection_id,
                );
                self.send_error_response(
                    "Connection pooler local resource exhausted. Please try again later.",
                    "53000",
                    err,
                ).await
            }
            Error::ServerUnavailableError(ref msg, _) => {
                log::warn!(
                    "[{}@{} #c{}] server unavailable (suppressed from client): {msg}",
                    self.username, self.pool_name, self.connection_id,
                );
                self.send_error_response(
                    "Server unavailable. Please try again later.",
                    "08006",
                    err,
                ).await
            }
            Error::QueryWaitTimeout => {
                self.send_error_response(
                    "Query wait timed out. The server may be overloaded.",
                    "57014",
                    err,
                ).await
            }
            Error::AllServersDown => {
                self.send_error_response(
                    "All database servers are currently unavailable. Please try again later.",
                    "08006",
                    err,
                ).await
            }
            Error::ShuttingDown => {
                self.send_error_response(
                    "Connection pooler is shutting down. Please reconnect in a few moments.",
                    "58006",
                    err,
                ).await
            }
            Error::FlushTimeout => {
                self.send_error_response(
                    "Timeout while sending data to client. Please check your network connection.",
                    "08006",
                    err,
                ).await
            }
            Error::ProxyTimeout => {
                self.send_error_response(
                    "Proxy operation timed out. Please try again later.",
                    "08006",
                    err,
                ).await
            }
            // every other Error variant used to fall
            // through to a silent `Err(err)`. We now emit an ErrorResponse
            // so drivers get a proper PG-protocol message AND log the
            // detailed error server-side. Do NOT include
            // the Display representation in the client-visible message,
            // because variants like `ServerStartupReadParameters(String)`
            // and `ParseBytesError(String)` carry connection strings,
            // file paths, and PG-side details that may be sensitive.
            // Log full details to pg_doorman logs; client sees a generic
            // sanitized message + the SQLSTATE for category dispatch.
            other => {
                log::error!(
                    "[{}@{} #c{}] pooler internal error: {other:?}",
                    self.username,
                    self.pool_name,
                    self.connection_id,
                );
                self.send_error_response(
                    "Pooler internal error. Check pg_doorman logs for details.",
                    "XX000",
                    other,
                )
                .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn generic_client_error_responses_are_deadline_bound() {
        let src = include_str!("error_handling.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let helper_start = impl_src
            .find("pub(crate) async fn send_error_response")
            .expect("send_error_response helper not found");
        let helper_body = &impl_src[helper_start..];
        let helper_end = helper_body
            .find("\n    pub(crate) async fn process_error")
            .expect("process_error should follow send_error_response");
        let helper_body = &helper_body[..helper_end];

        assert!(
            helper_body.contains("config_arc().general.proxy_copy_data_timeout.as_std()"),
            "generic client ErrorResponse writes must use proxy_copy_data_timeout"
        );
        assert!(
            helper_body.contains("error_response_timeout(&mut self.write"),
            "generic client ErrorResponse writes must be deadline-bound"
        );
        assert!(
            !helper_body.contains("error_response(&mut self.write"),
            "generic client ErrorResponse writes must not use bare write_all_flush"
        );
    }
}
