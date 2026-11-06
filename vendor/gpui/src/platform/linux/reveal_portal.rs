use std::fs::File;

use ashpd::{
    ActivationToken, Error, PortalError,
    desktop::{ResponseError, open_uri::OpenDirectoryRequest},
};

pub(super) async fn reveal_fallback_error(
    file: &File,
    activation_token: Option<String>,
) -> Option<Error> {
    let outcome = OpenDirectoryRequest::default()
        .activation_token(activation_token.map(ActivationToken::from))
        .send(file)
        .await
        .and_then(|request| request.response());
    fallback_error(outcome)
}

fn fallback_error(outcome: Result<(), Error>) -> Option<Error> {
    match outcome {
        Err(
            error @ (Error::Response(ResponseError::Other)
            | Error::PortalNotFound(_)
            | Error::RequiresVersion(_, _)
            | Error::Zbus(_)
            | Error::Portal(
                PortalError::ZBus(_) | PortalError::Failed(_) | PortalError::NotFound(_),
            )),
        ) => Some(error),
        // Successful, canceled, denied and unconfirmed requests must not open a second window.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_requires_a_failed_or_unavailable_portal() {
        assert!(fallback_error(Ok(())).is_none());
        assert!(fallback_error(Err(Error::Response(ResponseError::Cancelled))).is_none());
        assert!(
            fallback_error(Err(Error::Portal(PortalError::NotAllowed(String::new())))).is_none()
        );
        assert!(fallback_error(Err(Error::NoResponse)).is_none());
        assert!(fallback_error(Err(Error::Response(ResponseError::Other))).is_some());
        assert!(fallback_error(Err(Error::RequiresVersion(3, 2))).is_some());
    }
}
