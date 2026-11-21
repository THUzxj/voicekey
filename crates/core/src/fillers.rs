//! 口语卡顿词（filler words）管理与历史自动发现。
//!
//! - 合并内置基础词表、用户自定义词表与历史已采纳词表
//! - 通过字符级 LCS diff 对比多渠道原始文本与最终定稿，自动提取反复被删掉的 1–3 字短片段
//! - 持久化到 `data_dir()/fillers_learned.json`，供用户在设置页一键「采纳」或「忽略」
use crate::util::{data_file, now_ms, strip_punctuation};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

/// 温和的内置基础口语卡顿词表
pub const DEFAULT_FILLERS: &[&str] = &["呃", "嗯", "啊", "呃哪", "嗯嗯", "那个", "就是说"];

/// 历史自动发现达到该次数后，在设置页推荐给用户
pub const SUGGEST_MIN_COUNT: u32 = 3;
const MAX_ENTRIES: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FillerStatus {
    Suggested,
    Accepted,
    Ignored,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LearnedFiller {
    pub word: String,
    pub count: u32,
    pub last_seen_ms: u64,
    pub status: FillerStatus,
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    entries: Vec<LearnedFiller>,
}

static LOCK: Mutex<()> = Mutex::new(());

fn store_path() -> PathBuf {
    data_file("fillers_learned.json")
}

fn load_from(path: &PathBuf) -> Store {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_to(path: &PathBuf, store: &Store) {
    if let Ok(bytes) = serde_json::to_vec_pretty(store) {
        let _ = std::fs::write(path, bytes);
    }
}

/// 合并当前生效的口语卡顿词表：默认词表 + 用户自定义 + 历史已采纳（按去重后顺序返回）
pub fn effective_fillers(custom: &[String], include_learned: bool) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut push = |w: &str| {
        let w = w.trim();
        if !w.is_empty() && seen.insert(w.to_lowercase()) {
            out.push(w.to_string());
        }
    };

    for &w in DEFAULT_FILLERS {
        push(w);
    }
    for w in custom {
        push(w);
    }
    if include_learned {
        let _g = LOCK.lock().unwrap();
        let store = load_from(&store_path());
        for e in store.entries {
            if e.status == FillerStatus::Accepted {
                push(&e.word);
            }
        }
    }
    out
}

/// 判断文本中是否包含任意一个生效的口语卡顿词（中文按子串匹配，纯 ASCII 英文按词边界匹配）
pub fn contains_filler(text: &str, fillers: &[String]) -> bool {
    let lower = text.to_lowercase();
    for f in fillers {
        let f_trim = f.trim();
        if f_trim.is_empty() {
            continue;
        }
        let f_low = f_trim.to_lowercase();
        if f_low.is_ascii() {
            if contains_ascii_word(&lower, &f_low) {
                return true;
            }
        } else if lower.contains(&f_low) {
            return true;
        }
    }
    false
}

fn contains_ascii_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let nlen = needle.len();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let idx = start + pos;
        let left_ok = idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric();
        let right_ok = idx + nlen == bytes.len() || !bytes[idx + nlen].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

/// 通过字符级 LCS diff，找出 `source` 中存在、但在 `target` 中被删掉的 1–3 字连续短片段。
/// 用于从「多渠道原文 vs 最终定稿」的差异里自动发现说话人的口语卡顿词。
pub fn extract_deleted_fragments(source: &str, target: &str) -> Vec<String> {
    let s_clean = strip_punctuation(source);
    let t_clean = strip_punctuation(target);
    let a: Vec<char> = s_clean.chars().filter(|c| !c.is_whitespace()).collect();
    let b: Vec<char> = t_clean.chars().filter(|c| !c.is_whitespace()).collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 || n > 500 || m > 500 {
        return Vec::new();
    }

    // 标准 LCS DP 表
    let mut dp = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i].to_ascii_lowercase() == b[j].to_ascii_lowercase() {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    // 回溯找出 a 中未匹配的下标，并排除“替换”场景（只保留纯删除：b 侧对应位置没有同时插入新字）
    let mut kept_a = vec![false; n];
    let mut kept_b = vec![false; m];
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i].to_ascii_lowercase() == b[j].to_ascii_lowercase() {
            kept_a[i] = true;
            kept_b[j] = true;
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }

    // 扫描连续的纯删除段：仅当两侧均锚定在相同 LCS 位置（中间 b 没有插入字符）时视作纯删掉的口语词
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut ai = 0;
    let mut bi = 0;
    while ai < n {
        if kept_a[ai] {
            while bi < m && !kept_b[bi] {
                bi += 1;
            }
            if bi < m {
                bi += 1;
            }
            ai += 1;
            continue;
        }
        let a_start = ai;
        while ai < n && !kept_a[ai] {
            ai += 1;
        }
        let b_start = bi;
        while bi < m && !kept_b[bi] {
            bi += 1;
        }
        // b 侧没有对应插入，说明这段是纯删除而非同音字替换
        if bi == b_start {
            let frag: String = a[a_start..ai].iter().collect();
            let char_len = frag.chars().count();
            if (1..=3).contains(&char_len)
                && !frag.chars().all(|c| c.is_ascii_digit())
                && seen.insert(frag.to_lowercase())
            {
                out.push(frag);
            }
        }
    }
    out
}

/// 从一次会话的「多渠道原文列表 + 最终定稿」中学习高频被删除的口语短片段
pub fn learn_from_session(raw_texts: &[String], final_text: &str) {
    let mut session_words = HashSet::new();
    for raw in raw_texts {
        if raw.trim() == final_text.trim() {
            continue;
        }
        for w in extract_deleted_fragments(raw, final_text) {
            session_words.insert(w);
        }
    }
    if session_words.is_empty() {
        return;
    }
    let _g = LOCK.lock().unwrap();
    let path = store_path();
    let mut store = load_from(&path);
    let ts = now_ms();
    for word in session_words {
        if let Some(e) = store.entries.iter_mut().find(|e| e.word.eq_ignore_ascii_case(&word)) {
            e.count = e.count.saturating_add(1);
            e.last_seen_ms = ts;
        } else {
            store.entries.push(LearnedFiller {
                word,
                count: 1,
                last_seen_ms: ts,
                status: FillerStatus::Suggested,
            });
        }
    }
    // 保留全部 Accepted/Ignored，其余按频次和最近时间截断上限
    store.entries.sort_by(|a, b| {
        let rank = |s: FillerStatus| match s {
            FillerStatus::Accepted => 0,
            FillerStatus::Suggested => 1,
            FillerStatus::Ignored => 2,
        };
        rank(a.status)
            .cmp(&rank(b.status))
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| b.last_seen_ms.cmp(&a.last_seen_ms))
    });
    if store.entries.len() > MAX_ENTRIES {
        store.entries.truncate(MAX_ENTRIES);
    }
    save_to(&path, &store);
}

/// 列出可展示给用户的历史口语词：全部 Accepted/Ignored，以及频次达到阈值的 Suggested
pub fn list_learned() -> Vec<LearnedFiller> {
    let _g = LOCK.lock().unwrap();
    let store = load_from(&store_path());
    store
        .entries
        .into_iter()
        .filter(|e| e.status != FillerStatus::Suggested || e.count >= SUGGEST_MIN_COUNT)
        .collect()
}

/// 更新某个历史口语词的状态（采纳 / 忽略 / 恢复待定）
pub fn set_status(word: &str, status: FillerStatus) -> Vec<LearnedFiller> {
    let _g = LOCK.lock().unwrap();
    let path = store_path();
    let mut store = load_from(&path);
    if let Some(e) = store.entries.iter_mut().find(|e| e.word.eq_ignore_ascii_case(word)) {
        e.status = status;
    }
    save_to(&path, &store);
    store
        .entries
        .into_iter()
        .filter(|e| e.status != FillerStatus::Suggested || e.count >= SUGGEST_MIN_COUNT)
        .collect()
}

/// 清空历史自动发现记录
pub fn clear_learned() {
    let _g = LOCK.lock().unwrap();
    let path = store_path();
    save_to(&path, &Store::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_pure_deleted_filler_but_ignores_substitutions() {
        // 「灰 -> 会」是替换，不应提取；「呃那个」是纯删除，应提取
        let raw = "今天下午开灰之后呃那个我们再确认一下";
        let fin = "今天下午开会之后我们再确认一下";
        let frags = extract_deleted_fragments(raw, fin);
        assert_eq!(frags, vec!["呃那个"]);
    }

    #[test]
    fn contains_filler_handles_cjk_and_ascii_word_boundary() {
        let fillers = vec!["那个".into(), "um".into()];
        assert!(contains_filler("明天上午那个我们再确认一下", &fillers));
        assert!(contains_filler("well um I think so", &fillers));
        assert!(!contains_filler("summary and forum", &fillers));
    }
}
