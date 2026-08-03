//! 本地数据静态加密（AINS_PLAN 7.5 隐私审计）。
//!
//! [`EncryptedKvStore`] 是 [`KvStore`] 的透明装饰器：value 载荷在写入底层
//! 存储前用 ChaCha20-Poly1305（AEAD）加密，读取后解密。设计约束：
//! - **key 保持明文**：前缀列举 / 删除、TTL 清理依赖底层 key 有序与元数据，
//!   故仅加密 value；key 命名不应含敏感信息（与常规静态加密实践一致）。
//! - **AAD 绑定 key**：附加认证数据取该条目的存储 key，密文被跨 key 搬运时
//!   解密（认证）失败，防止“密文搬运”篡改。
//! - **AEAD 完整性**：错误密钥 / 密文被篡改 / nonce 被改都会导致解密失败
//!   （而非静默返回错误明文）。
//! - **随机 nonce**：每次写入随机 96-bit nonce（getrandom CSPRNG，双 target）。
//! - **密文长度泄露**：AEAD 无 padding，`seal` 输出长度 = 明文 + 16B tag
//!   （再经 base64）；对 JSON 条目可推断规模。属静态加密已知局限，不做
//!   padding（会显著膨胀小型 KV 条目），需要时由上层自行处理。
//!
//! **使用边界**：本装饰器**仅限单表（单 key 空间）使用**。AAD 不区分表名——
//! 若把同一 [`EncryptionKey`] 包在共享 key 集合的多张表之上（如 `memories` 与
//! `embeddings` 使用相同的 `namespace.storage_key(id)`），同 key 密文可跨表
//! 认证通过（完整性受损）。需要多表加密时应把表名并入 AAD 或为每表独立密钥。
//!
//! 密钥（[`EncryptionKey`]）可随机生成（首次运行生成、由调用方安全持久化）、
//! 由外部密钥字节导入、或由用户口令经 Argon2id 派生。密钥在 Drop 时清零。

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use serde_json::{Value, json};
use zeroize::Zeroize;

use crate::error::MemoryError;
use crate::memory::kv::KvStore;

/// ChaCha20-Poly1305 密钥长度（256-bit）。
const KEY_LEN: usize = 32;
/// ChaCha20-Poly1305 nonce 长度（96-bit）。
const NONCE_LEN: usize = 12;
/// 加密信封版本标记字段（区分密文信封与明文，兼容未来格式演进）。
const SEALED_MARKER: &str = "__ains_sealed_v";
/// 当前加密信封格式版本。
const SEALED_VERSION: u64 = 1;
/// Argon2 要求的最小 salt 长度（字节）。NIST SP 800-63B 建议 ≥16，
/// getrandom 可用时无合理理由缩减。
const MIN_SALT_LEN: usize = 16;

/// 256-bit 本地加密密钥；Drop 时清零，Debug 不泄露密钥内容。
pub struct EncryptionKey([u8; KEY_LEN]);

impl EncryptionKey {
    /// 由外部管理的 32 字节密钥导入（如系统钥匙串 / 安全存储读取）。
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// 随机生成新密钥（首次运行生成，需由调用方安全持久化后续复用）。
    pub fn generate() -> Result<Self, MemoryError> {
        let mut bytes = [0u8; KEY_LEN];
        if let Err(e) = getrandom::getrandom(&mut bytes) {
            // 失败路径也可能已写入部分随机字节：显式清零后丢弃。
            bytes.zeroize();
            return Err(MemoryError::Encryption(format!(
                "key generation RNG failed: {e}"
            )));
        }
        Ok(Self(bytes))
    }

    /// 由用户口令 + salt 经 Argon2id 派生密钥（口令场景）。
    ///
    /// `salt` 至少 [`MIN_SALT_LEN`] 字节，非机密但需持久化以便复现同一密钥；
    /// 相同 (口令, salt) 恒得同一密钥。
    pub fn from_passphrase(passphrase: &str, salt: &[u8]) -> Result<Self, MemoryError> {
        if salt.len() < MIN_SALT_LEN {
            return Err(MemoryError::Encryption(format!(
                "salt too short: need >= {MIN_SALT_LEN} bytes, got {}",
                salt.len()
            )));
        }
        let mut bytes = [0u8; KEY_LEN];
        if let Err(e) =
            argon2::Argon2::default().hash_password_into(passphrase.as_bytes(), salt, &mut bytes)
        {
            // 失败路径可能已写入部分派生字节：显式清零后丢弃。
            bytes.zeroize();
            return Err(MemoryError::Encryption(format!(
                "key derivation failed: {e}"
            )));
        }
        Ok(Self(bytes))
    }

    fn cipher(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(Key::from_slice(&self.0))
    }

    /// 加密一条 value 为密文信封（AAD = `storage_key`，绑定条目位置）。
    pub fn seal(&self, storage_key: &str, value: &Value) -> Result<Value, MemoryError> {
        let plaintext =
            serde_json::to_vec(value).map_err(|e| MemoryError::Serialization(e.to_string()))?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce_bytes)
            .map_err(|e| MemoryError::Encryption(format!("nonce RNG failed: {e}")))?;
        let ciphertext = self
            .cipher()
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &plaintext,
                    aad: storage_key.as_bytes(),
                },
            )
            .map_err(|_| MemoryError::Encryption("AEAD encryption failed".into()))?;
        Ok(json!({
            SEALED_MARKER: SEALED_VERSION,
            "n": B64.encode(nonce_bytes),
            "c": B64.encode(ciphertext),
        }))
    }

    /// 解密一条密文信封回原 value（AAD = `storage_key`，须与加密时一致）。
    pub fn unseal(&self, storage_key: &str, sealed: &Value) -> Result<Value, MemoryError> {
        let obj = sealed.as_object().ok_or_else(|| {
            MemoryError::Encryption("stored value is not an AINS-sealed envelope".into())
        })?;
        let version = obj.get(SEALED_MARKER).and_then(Value::as_u64);
        if version != Some(SEALED_VERSION) {
            return Err(MemoryError::Encryption(format!(
                "unrecognized sealed envelope (version {version:?})"
            )));
        }
        let nonce_bytes = obj
            .get("n")
            .and_then(Value::as_str)
            .ok_or_else(|| MemoryError::Encryption("sealed envelope missing nonce".into()))
            .and_then(|s| {
                B64.decode(s)
                    .map_err(|e| MemoryError::Encryption(format!("bad nonce b64: {e}")))
            })?;
        if nonce_bytes.len() != NONCE_LEN {
            return Err(MemoryError::Encryption("invalid nonce length".into()));
        }
        let ciphertext = obj
            .get("c")
            .and_then(Value::as_str)
            .ok_or_else(|| MemoryError::Encryption("sealed envelope missing ciphertext".into()))
            .and_then(|s| {
                B64.decode(s)
                    .map_err(|e| MemoryError::Encryption(format!("bad ciphertext b64: {e}")))
            })?;
        let plaintext = self
            .cipher()
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &ciphertext,
                    aad: storage_key.as_bytes(),
                },
            )
            // AEAD 失败不区分“错误密钥 / 篡改 / AAD 不匹配”，统一报解密失败。
            .map_err(|_| {
                MemoryError::Encryption("decryption failed (wrong key or corrupted data)".into())
            })?;
        serde_json::from_slice(&plaintext).map_err(|e| MemoryError::Serialization(e.to_string()))
    }
}

impl Drop for EncryptionKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for EncryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不泄露密钥字节
        f.write_str("EncryptionKey(***)")
    }
}

/// [`KvStore`] 静态加密装饰器：value 经 ChaCha20-Poly1305 加密后写入底层，
/// 读取时解密。key / TTL 元数据保持明文（见模块文档）。
///
/// **部署迁移提示**：对已含明文数据的底层存储启用本装饰器后，旧明文条目
/// 的 `get` 会报解密错误（[`EncryptionKey::unseal`] 拒绝误解码，安全设计）。
/// 启用前需一次性迁移：先读明文 → 以同一 key 写回（经装饰器加密）。
pub struct EncryptedKvStore {
    inner: Arc<dyn KvStore>,
    key: EncryptionKey,
}

impl EncryptedKvStore {
    pub fn new(inner: Arc<dyn KvStore>, key: EncryptionKey) -> Self {
        Self { inner, key }
    }
}

impl std::fmt::Debug for EncryptedKvStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedKvStore").finish_non_exhaustive()
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl KvStore for EncryptedKvStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
        match self.inner.get(key).await? {
            Some(sealed) => Ok(Some(self.key.unseal(key, &sealed)?)),
            None => Ok(None),
        }
    }

    async fn set(
        &self,
        key: &str,
        value: &Value,
        ttl: Option<Duration>,
    ) -> Result<(), MemoryError> {
        let sealed = self.key.seal(key, value)?;
        self.inner.set(key, &sealed, ttl).await
    }

    async fn delete(&self, key: &str) -> Result<(), MemoryError> {
        self.inner.delete(key).await
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
        // key 明文，直接透传（前缀语义不受 value 加密影响）。
        self.inner.list_prefix(prefix).await
    }

    async fn sweep_expired(&self) -> Result<u64, MemoryError> {
        // TTL 存于底层明文信封，清理无需解密。
        self.inner.sweep_expired().await
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<u64, MemoryError> {
        self.inner.delete_prefix(prefix).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> EncryptionKey {
        EncryptionKey::from_bytes([7u8; KEY_LEN])
    }

    #[test]
    fn seal_unseal_roundtrip() {
        let key = test_key();
        let value = json!({"content": "secret memory", "importance": 3.5, "tags": ["a", "b"]});
        let sealed = key.seal("personal/mem-1", &value).unwrap();
        let back = key.unseal("personal/mem-1", &sealed).unwrap();
        assert_eq!(back, value);
    }

    #[test]
    fn sealed_envelope_hides_plaintext() {
        let key = test_key();
        let value = json!({"content": "TOP-SECRET-TOKEN"});
        let sealed = key.seal("k", &value).unwrap();
        // 密文信封序列化后不得包含明文关键字
        let serialized = serde_json::to_string(&sealed).unwrap();
        assert!(!serialized.contains("TOP-SECRET-TOKEN"));
        assert!(serialized.contains(SEALED_MARKER));
    }

    #[test]
    fn wrong_key_fails_decryption() {
        let value = json!({"x": 1});
        let sealed = EncryptionKey::from_bytes([1u8; KEY_LEN])
            .seal("k", &value)
            .unwrap();
        let err = EncryptionKey::from_bytes([2u8; KEY_LEN])
            .unseal("k", &sealed)
            .unwrap_err();
        assert!(matches!(err, MemoryError::Encryption(_)));
    }

    #[test]
    fn aad_binds_ciphertext_to_storage_key() {
        let key = test_key();
        let sealed = key.seal("key-A", &json!({"x": 1})).unwrap();
        // 用不同 storage_key 解密同一密文 → AAD 不匹配 → 失败（防密文搬运）
        let err = key.unseal("key-B", &sealed).unwrap_err();
        assert!(matches!(err, MemoryError::Encryption(_)));
        // 用正确 key 解密成功
        assert_eq!(key.unseal("key-A", &sealed).unwrap(), json!({"x": 1}));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let key = test_key();
        let mut sealed = key.seal("k", &json!({"v": "abc"})).unwrap();
        // 篡改密文末字节
        let c = sealed["c"].as_str().unwrap();
        let mut raw = B64.decode(c).unwrap();
        *raw.last_mut().unwrap() ^= 0x01;
        sealed["c"] = json!(B64.encode(&raw));
        assert!(matches!(
            key.unseal("k", &sealed).unwrap_err(),
            MemoryError::Encryption(_)
        ));
    }

    #[test]
    fn plaintext_value_is_not_accepted_as_sealed() {
        let key = test_key();
        // 底层若存的是明文（非信封），unseal 应报错而非误解码
        let err = key.unseal("k", &json!({"content": "plain"})).unwrap_err();
        assert!(matches!(err, MemoryError::Encryption(_)));
    }

    #[test]
    fn passphrase_derivation_is_deterministic_and_salt_sensitive() {
        let salt_a = b"salt-fixed-xyz-001";
        let k1 = EncryptionKey::from_passphrase("correct horse", salt_a).unwrap();
        let k2 = EncryptionKey::from_passphrase("correct horse", salt_a).unwrap();
        // 同口令同 salt → 同密钥（可互相解密）
        let sealed = k1.seal("k", &json!({"n": 42})).unwrap();
        assert_eq!(k2.unseal("k", &sealed).unwrap(), json!({"n": 42}));
        // 不同 salt → 不同密钥（解密失败）
        let k3 = EncryptionKey::from_passphrase("correct horse", b"salt-other-99-extra").unwrap();
        assert!(k3.unseal("k", &sealed).is_err());
    }

    #[test]
    fn short_salt_is_rejected() {
        assert!(matches!(
            EncryptionKey::from_passphrase("pw", b"short").unwrap_err(),
            MemoryError::Encryption(_)
        ));
    }

    #[test]
    fn generate_produces_distinct_usable_keys() {
        let k1 = EncryptionKey::generate().unwrap();
        let sealed = k1.seal("k", &json!({"a": 1})).unwrap();
        assert_eq!(k1.unseal("k", &sealed).unwrap(), json!({"a": 1}));
        // 另一随机密钥极大概率不同 → 无法解密
        let k2 = EncryptionKey::generate().unwrap();
        assert!(k2.unseal("k", &sealed).is_err());
    }

    #[test]
    fn debug_does_not_leak_key() {
        let key = EncryptionKey::from_bytes([9u8; KEY_LEN]);
        assert_eq!(format!("{key:?}"), "EncryptionKey(***)");
    }
}
