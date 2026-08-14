//! Phase 4/5 Web 端契约测试（wasm32 + headless 浏览器，CI wasm-pack 执行）。
//!
//! 覆盖 context / perception / model_service 中双 target 编译的纯函数，验证
//! 其在浏览器 WASM 环境下的行为与 Native 一致（补齐既有仅 native 单测 +
//! wasm clippy 的运行时空档，对齐 Phase 2/3 的 `web_*.rs` 模式）。

#![cfg(target_arch = "wasm32")]

use wasm_bindgen_test::*;

use rust_agent::context::compact::{
    estimate_message_tokens, estimate_tokens, get_autocompact_threshold,
};
use rust_agent::context::prompt_pipeline::{
    PromptPipelineInput, PromptSections, build_system_prompt, permission_mode_section,
    skills_section,
};
use rust_agent::kernel::messages::{ContentBlock, ConversationMessage, Role};
use rust_agent::model_client::ModelRequest;
use rust_agent::model_service::{
    ToolTagFilter, build_ai_request, detect_audio_format, parse_assistant_content,
};
use rust_agent::perception::{FileChannel, PerceptionOutcome};
use rust_agent::policy::PermissionMode;
use rust_agent::skills::SkillSummary;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn compact_token_estimation_matches_native() {
    assert_eq!(estimate_tokens(""), 0);
    assert_eq!(estimate_tokens("abcd"), 1);
    assert_eq!(estimate_tokens("abcde"), 2);
    // 单张图像：3072 * 4/3 = 4096
    let with_image = vec![ConversationMessage {
        role: Role::User,
        content: vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        }],
    }];
    assert_eq!(estimate_message_tokens(&with_image), 4096);
    // 200000 - 20000 - 13000
    assert_eq!(get_autocompact_threshold(None), 167_000);
}

#[wasm_bindgen_test]
fn prompt_pipeline_sections_toggle_on_web() {
    assert!(permission_mode_section(PermissionMode::Plan).contains("Plan mode is enabled"));
    let skills = vec![SkillSummary {
        name: "pdf".into(),
        description: "PDF".into(),
        category: "docs".into(),
        requires_tools: vec![],
    }];
    // 索引段输出为数据记录格式（name/description 均 Debug 引号化，见
    // prompt_pipeline::skills_section）：与 native 单测断言保持一致。
    assert!(
        skills_section(&skills)
            .unwrap()
            .contains(r#"- name="pdf"; description="PDF""#)
    );

    let input = PromptPipelineInput {
        cwd: std::path::Path::new("/tmp/ains-web-none"),
        now_ms: 1_609_459_200_000,
        permission_mode: PermissionMode::Default,
        skills: &[],
        memory_prompt: None,
        custom_base: None,
        environment_shell: None,
        environment_git_branch: None,
    };
    // 仅 Environment 段（Web 无本地文件系统项目指令，project_docs 段自然为空）
    let prompt = build_system_prompt(
        &input,
        PromptSections {
            base: false,
            permission_mode: false,
            skills: false,
            project_docs: false,
            environment: true,
            memory: false,
        },
    );
    assert!(prompt.starts_with("# Environment"));
    assert!(prompt.contains("- Date: 2021-01-01"));
}

#[wasm_bindgen_test]
fn perception_file_text_ingest_and_event_on_web() {
    // 文本文件解析在双端一致（PDF 仅 Native，不在此覆盖）
    let outcome = FileChannel::new()
        .ingest(b"web notes".to_vec(), "notes.txt", None)
        .unwrap();
    assert_eq!(outcome.text.as_deref(), Some("web notes"));
    assert_eq!(outcome.source_note.as_deref(), Some("[file: notes.txt]"));

    let event = PerceptionOutcome::from_text("hi")
        .into_agent_event(Some("q"))
        .expect("event");
    match event {
        rust_agent::kernel::AgentEvent::UserMessage { content, .. } => {
            assert!(content.contains("q"));
            assert!(content.contains("hi"));
        }
        _ => panic!("expected UserMessage"),
    }
}

#[wasm_bindgen_test]
fn model_service_protocol_helpers_on_web() {
    // 协议块解析
    let blocks =
        parse_assistant_content("答案\n<tool_use id=\"c1\" name=\"calc\">\n{\"a\":1}\n</tool_use>");
    assert!(
        blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    );

    // UI delta 过滤：跨分片抑制协议片段
    let mut filter = ToolTagFilter::new();
    let mut visible = String::new();
    visible.push_str(&filter.push("前 <tool"));
    visible.push_str(&filter.push("_use id=\"c1\" name=\"c\">{}</tool_"));
    visible.push_str(&filter.push("use> 后"));
    visible.push_str(&filter.flush());
    assert_eq!(visible, "前  后");

    // 音频魔数嗅探
    assert_eq!(detect_audio_format(b"RIFF....WAVE"), "wav");
    assert_eq!(detect_audio_format(b"OggS...."), "ogg");

    // 工具协议注入 system prompt
    let request = ModelRequest {
        system_prompt: Some("base".into()),
        tools: vec![rust_agent::tools::ToolDef {
            name: "calc".into(),
            description: "calc".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        ..Default::default()
    };
    let ai_request = build_ai_request(&request);
    let instructions = ai_request.instructions.unwrap();
    assert!(instructions.contains("# Tool Call Protocol"));
    assert!(instructions.contains("- calc: calc"));
}
