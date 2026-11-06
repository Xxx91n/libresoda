//! 账号态探测（对应上游 `soda/account.go`）。

use super::quality::is_preview;
use super::types::{VIP_PROBE_TRACK_ID, VIP_PROBE_TRACK_URL};
use super::Soda;
use crate::error::{Result, SodaError};
use crate::model::{Song, SOURCE_SODA};

/// 等价 `IsVipAccount`：用一首已知的 VIP 曲目探测当前 Cookie 是否能拿到完整流。
pub fn is_vip_account(soda: &Soda) -> Result<bool> {
    if let Some(cached) = soda.cached_vip() {
        return Ok(cached);
    }

    if !soda.has_cookie() {
        soda.set_cached_vip(false);
        return Ok(false);
    }

    // 首选：官方 `/luna/pc/me` 的 `my_info.is_vip`。
    // 这是账号自身的会员标记，不受"某首歌是否可播/是否需要签名"影响；
    // 早先按"探测曲目能否拿到完整流"判断会把 SVIP 误判为非会员（实测 is_vip=true 却返回 false）。
    if let Ok(me) = super::user_playlist::fetch_pc_me(soda) {
        let is_vip = me.my_info.is_vip
            || matches!(
                me.my_info.vip_stage.trim().to_lowercase().as_str(),
                "vip" | "svip"
            );
        soda.set_cached_vip(is_vip);
        return Ok(is_vip);
    }

    let probe = Song {
        id: VIP_PROBE_TRACK_ID.to_string(),
        source: SOURCE_SODA.to_string(),
        link: VIP_PROBE_TRACK_URL.to_string(),
        extra: crate::util::extra_from_pairs([("track_id", VIP_PROBE_TRACK_ID)]),
        ..Default::default()
    };

    match super::download::get_download_info(soda, &probe) {
        Ok(info) => {
            let is_vip = !info.url.is_empty() && !is_preview(&info, 180);
            soda.set_cached_vip(is_vip);
            Ok(is_vip)
        }
        Err(err) => {
            if probe_failure_means_not_vip(&err) {
                soda.set_cached_vip(false);
                return Ok(false);
            }
            Err(err)
        }
    }
}

/// 探测失败能否判定「不是会员」。
///
/// 签名故障优先：签名器 stderr / 自定义提供者的文案里可能碰巧带
/// `requires cookie` 之类子串，但那是服务故障，不能缓存成"非会员"（#36）。
fn probe_failure_means_not_vip(err: &SodaError) -> bool {
    !err.is_signer_failure() && err.is_missing_entitlement()
}

impl Soda {
    /// 等价 `(*Soda).IsVipAccount`。
    pub fn is_vip_account(&self) -> Result<bool> {
        is_vip_account(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signer_failure_is_not_treated_as_missing_entitlement() {
        let signer = SodaError::http("signer error: requires cookie");
        assert!(signer.is_missing_entitlement(), "子串确实会命中");
        assert!(
            !probe_failure_means_not_vip(&signer),
            "签名故障不能被缓存成非会员"
        );
        assert!(probe_failure_means_not_vip(&SodaError::http(
            "feed requires cookie"
        )));
    }
}
