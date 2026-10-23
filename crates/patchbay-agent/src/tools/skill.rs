//! Skill: load one of the user's Claude Code skills into the conversation.

use async_trait::async_trait;
use patchbay_llm::ToolSpec;
use serde_json::Value;

use super::*;

pub struct Skill;

#[async_trait]
impl Tool for Skill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Skill".into(),
            description: "Load an installed skill: its instructions and the directory its files are in. The system \
prompt lists the skills with what each is for. Load one when the task matches its description, before you \
start, then follow it."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"skill": {"type": "string", "description": "The skill's name, as listed"}},
                "required": ["skill"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "read"
    }

    fn read_only(&self) -> bool {
        true
    }

    fn title(&self, input: &Value) -> String {
        format!("Skill: {}", str_arg(input, "skill").unwrap_or("?"))
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(name) = str_arg(&input, "skill").map(|s| s.trim().trim_start_matches('/')) else {
            return ToolOutput::error("skill is required");
        };
        let setup = ctx.session.claude();
        match setup.skills.iter().find(|s| s.name == name) {
            Some(s) => ToolOutput::text(crate::claude::skill_text(s)),
            None => {
                let names: Vec<&str> = setup.skills.iter().map(|s| s.name.as_str()).collect();
                ToolOutput::error(format!("no skill named {name}; installed: {}", names.join(", ")))
            }
        }
    }
}
