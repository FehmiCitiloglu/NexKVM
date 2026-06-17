use async_trait::async_trait;
use nexkvm_input::{InputCapture, InputError, InputEvent};

#[derive(Debug, Clone, Copy)]
pub struct MacosInputCapture {
    accessibility_trusted: bool,
}

impl MacosInputCapture {
    #[must_use]
    pub fn new(accessibility_trusted: bool) -> Self {
        Self {
            accessibility_trusted,
        }
    }
}

#[async_trait]
impl InputCapture for MacosInputCapture {
    async fn next_event(&self) -> Result<InputEvent, InputError> {
        if !self.accessibility_trusted {
            return Err(InputError::PermissionDenied);
        }
        Err(InputError::Backend(
            "macOS CGEvent tap capture loop is not running".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capture_refuses_without_accessibility_permission() {
        let capture = MacosInputCapture::new(false);
        let result = capture.next_event().await;

        assert!(matches!(result, Err(InputError::PermissionDenied)));
    }
}
