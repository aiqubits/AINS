//! Skills 管理视图（Phase 6.4）：浏览 / 详情 / 删除，无导入入口。

use std::sync::Arc;

use dioxus::prelude::*;

use rust_agent::skills::{KvSkillStore, SkillEntry, SkillLoader, SkillManage, SkillTrust};
use ui::{I18nContext, SkillCard, SkillDetailView, SkillTrustView, SkillsPanel};

use crate::agent::service;

fn to_trust_view(trust: SkillTrust) -> SkillTrustView {
    match trust {
        SkillTrust::System => SkillTrustView::System,
        SkillTrust::Trusted => SkillTrustView::Trusted,
        SkillTrust::Generated => SkillTrustView::Generated,
        SkillTrust::Temporary => SkillTrustView::Temporary,
    }
}

/// Unix 毫秒 → `YYYY-MM-DD` 展示（面板粒度足够，避免时区库依赖）。
fn format_created_at(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn to_card(entry: &SkillEntry) -> SkillCard {
    match &entry.meta {
        Some(meta) => SkillCard {
            name: entry.name.clone(),
            description: meta.description.clone(),
            category: meta.category.clone(),
            trust: to_trust_view(meta.trust_level),
            created_at: format_created_at(meta.created_at),
            requires_tools: meta.requires_tools.clone(),
            corrupted: entry.corrupted,
        },
        None => SkillCard {
            name: entry.name.clone(),
            description: String::new(),
            category: "-".into(),
            trust: SkillTrustView::Generated,
            created_at: "-".into(),
            requires_tools: vec![],
            corrupted: true,
        },
    }
}

#[component]
pub fn Skills() -> Element {
    let i18n = use_context::<I18nContext>();
    let t = i18n.t();

    let mut store_sig = use_signal(|| None::<Arc<KvSkillStore>>);
    let mut cards = use_signal(Vec::<SkillCard>::new);
    let mut detail = use_signal(|| None::<SkillDetailView>);
    let mut error = use_signal(|| None::<String>);

    // 打开存储并加载列表（共享 KvStore 单例，不拉起 Kernel）
    use_future(move || async move {
        match service::open_skill_store().await {
            Ok(store) => {
                match store.list_entries().await {
                    Ok(entries) => {
                        cards.set(entries.iter().map(to_card).collect());
                        // 成功后清除历史错误横幅，避免瞬时故障文案常驻
                        error.set(None);
                    }
                    Err(err) => error.set(Some(format!("{}: {err}", t.skills_load_failed))),
                }
                store_sig.set(Some(store));
            }
            Err(err) => error.set(Some(format!("{}: {err}", t.skills_load_failed))),
        }
    });

    let reload = move || async move {
        let store = store_sig.read().clone();
        if let Some(store) = store {
            match store.list_entries().await {
                Ok(entries) => {
                    cards.set(entries.iter().map(to_card).collect());
                    error.set(None);
                }
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_load_failed))),
            }
        }
    };

    let on_open_detail = move |name: String| {
        let Some(store) = store_sig.read().clone() else {
            return;
        };
        spawn(async move {
            match store.load(&name).await {
                Ok(content) => {
                    let frontmatter = match &content.frontmatter {
                        serde_yaml::Value::Null => String::new(),
                        value => serde_yaml::to_string(value).unwrap_or_default(),
                    };
                    // 版本面（6.9）：可回滚候选 + 活跃版本（头指针权威源；
                    // 未版本化旧数据为空）
                    let rollback_versions =
                        store.rollback_candidates(&name).await.unwrap_or_default();
                    let active_version = store
                        .active_version(&name)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    detail.set(Some(SkillDetailView {
                        name,
                        frontmatter,
                        body: content.body,
                        rollback_versions,
                        active_version,
                    }));
                    error.set(None);
                }
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_load_failed))),
            }
        });
    };

    // 回滚（6.9）：目标版本内容提升为新大版本，刷新列表与详情
    let on_rollback = move |(name, version): (String, String)| {
        let Some(store) = store_sig.read().clone() else {
            return;
        };
        spawn(async move {
            match store.rollback_skill(&name, &version).await {
                Ok(_) => {
                    detail.set(None);
                    reload().await;
                }
                // 回滚失败用专属文案（非“加载失败”），候选过期等
                // 存储层错误对用户可归因
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_rollback_failed))),
            }
        });
    };

    let on_delete = move |name: String| {
        let Some(store) = store_sig.read().clone() else {
            return;
        };
        spawn(async move {
            match store.delete_skill(&name).await {
                Ok(()) => {
                    // 详情抽屉若正展示被删条目则一并关闭
                    if detail.read().as_ref().is_some_and(|d| d.name == name) {
                        detail.set(None);
                    }
                    reload().await;
                }
                Err(err) => error.set(Some(format!("{}: {err}", t.skills_delete_failed))),
            }
        });
    };

    rsx! {
        div { style: "padding:16px;",
            if let Some(err) = error.read().as_ref() {
                div { style: "margin-bottom:12px;color:var(--color-error-text);font-size:13px;",
                    "{err}"
                }
            }
            SkillsPanel {
                skills: cards,
                detail,
                on_open_detail,
                on_close_detail: move |_| detail.set(None),
                on_delete,
                on_rollback,
            }
        }
    }
}
