//! 运行平台标识，用于 Skills 门控、工具注册等平台感知逻辑。

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Web,
    Desktop,
    Mobile,
}

impl Platform {
    pub fn current() -> Self {
        #[cfg(target_arch = "wasm32")]
        {
            Self::Web
        }
        #[cfg(all(
            not(target_arch = "wasm32"),
            any(target_os = "android", target_os = "ios")
        ))]
        {
            Self::Mobile
        }
        #[cfg(all(
            not(target_arch = "wasm32"),
            not(any(target_os = "android", target_os = "ios"))
        ))]
        {
            Self::Desktop
        }
    }
}
