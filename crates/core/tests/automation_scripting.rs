//! End-to-end automation: a device-aware shortcut fires a rule whose planned
//! script action is executed through the sans-engine `ScriptEngine` boundary,
//! with language support and device context enforced — all via the public API.

use std::sync::Mutex;

use async_trait::async_trait;
use nexkvm_core::{
    AutomationAction, AutomationEngine, AutomationRule, AutomationTrigger, DeviceId, ScriptContext,
    ScriptEngine, ScriptError, ScriptLanguage, ScriptRef, ShortcutId,
};

/// Records every script it runs; refuses JavaScript to model a build that only
/// compiled the Lua backend.
#[derive(Default)]
struct RecordingScriptEngine {
    ran: Mutex<Vec<(String, AutomationTrigger)>>,
}

#[async_trait]
impl ScriptEngine for RecordingScriptEngine {
    fn supports(&self, language: ScriptLanguage) -> bool {
        matches!(language, ScriptLanguage::Lua)
    }

    async fn run(&self, script: &ScriptRef, ctx: ScriptContext) -> Result<(), ScriptError> {
        if !self.supports(script.language) {
            return Err(ScriptError::UnsupportedLanguage);
        }
        self.ran
            .lock()
            .expect("lock")
            .push((script.id.clone(), ctx.trigger));
        Ok(())
    }
}

#[tokio::test]
async fn shortcut_plans_and_runs_scripted_action() {
    let device = DeviceId::generate();
    let mut engine = AutomationEngine::new();
    engine
        .upsert(AutomationRule {
            id: "capture".into(),
            name: "Capture screenshot".into(),
            trigger: AutomationTrigger::ShortcutPressed {
                device,
                shortcut: ShortcutId::new("screenshot"),
            },
            action: AutomationAction::RunScript(ScriptRef::new(
                "screenshot.lua",
                ScriptLanguage::Lua,
            )),
            enabled: true,
        })
        .expect("valid rule");

    let trigger = AutomationTrigger::ShortcutPressed {
        device,
        shortcut: ShortcutId::new("screenshot"),
    };
    let plans = engine.plan(&trigger);
    assert_eq!(plans.len(), 1);

    let script_engine = RecordingScriptEngine::default();
    for plan in plans {
        if let AutomationAction::RunScript(script) = plan.action {
            script_engine
                .run(
                    &script,
                    ScriptContext {
                        rule_id: plan.rule_id,
                        trigger: trigger.clone(),
                    },
                )
                .await
                .expect("lua script runs");
        }
    }

    let ran = script_engine.ran.lock().expect("lock");
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].0, "screenshot.lua");
}

#[tokio::test]
async fn unsupported_language_is_rejected() {
    let script_engine = RecordingScriptEngine::default();
    let result = script_engine
        .run(
            &ScriptRef::new("macro.js", ScriptLanguage::JavaScript),
            ScriptContext {
                rule_id: "x".into(),
                trigger: AutomationTrigger::CommandInvoked(nexkvm_core::CommandId::new("noop")),
            },
        )
        .await;
    assert!(matches!(result, Err(ScriptError::UnsupportedLanguage)));
    assert!(script_engine.ran.lock().expect("lock").is_empty());
}
