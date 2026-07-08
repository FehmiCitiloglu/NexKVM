use crate::accessibility::AccessibilityStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosPermissionState {
    Ready,
    PermissionRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacosInputPermissionReport {
    pub accessibility: MacosPermissionState,
    pub can_capture_input: bool,
    pub can_inject_input: bool,
    pub next_step: Option<&'static str>,
}

pub fn input_permission_report(
    accessibility: &dyn AccessibilityStatus,
) -> MacosInputPermissionReport {
    if accessibility.is_trusted() {
        MacosInputPermissionReport {
            accessibility: MacosPermissionState::Ready,
            can_capture_input: true,
            can_inject_input: true,
            next_step: None,
        }
    } else {
        MacosInputPermissionReport {
            accessibility: MacosPermissionState::PermissionRequired,
            can_capture_input: false,
            can_inject_input: false,
            next_step: Some(
                "Grant Accessibility permission in System Settings > Privacy & Security > Accessibility",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct StubAccessibility(bool);

    impl AccessibilityStatus for StubAccessibility {
        fn is_trusted(&self) -> bool {
            self.0
        }

        fn prompt_and_check(&self) -> bool {
            self.0
        }
    }

    #[test]
    fn trusted_accessibility_enables_capture_and_injection() {
        let report = input_permission_report(&StubAccessibility(true));

        assert_eq!(report.accessibility, MacosPermissionState::Ready);
        assert!(report.can_capture_input);
        assert!(report.can_inject_input);
        assert_eq!(report.next_step, None);
    }

    #[test]
    fn missing_accessibility_reports_next_step() {
        let report = input_permission_report(&StubAccessibility(false));

        assert_eq!(
            report.accessibility,
            MacosPermissionState::PermissionRequired
        );
        assert!(!report.can_capture_input);
        assert!(!report.can_inject_input);
        assert_eq!(
            report.next_step,
            Some(
                "Grant Accessibility permission in System Settings > Privacy & Security > Accessibility"
            )
        );
    }
}
