//! 联想（#89）：上屏以后不用打字，先给出接下来最可能打的词。
//!
//! 用字级神经语言模型（[`crate::nnlm`]，微信输入法的模型，用户本机有才有）照光标前的整句往下接：
//! 一个字一个字地束搜索，只沿着词库里两到四个字的词走，词的分数是模型给这几个字的 ln P，
//! 再乘上「词到这里就完了」的概率（不然「不知」总比「不知道」高）。模型觉得这里多半该打标点了
//! （一句说完了），或者最好的词也没多大把握，就什么都不给。用户自己常连着用的词（输入习惯）
//! 加分、总给。没有模型就不联想。

use std::ops::Range;

use crate::autospace::is_han;
use crate::learn::{decayed, PAIR_BONUS_CAP, PAIR_WEIGHT};
use crate::lexicon::Lexicon;
use crate::nnlm::CharLm;
use crate::Engine;

/// 束搜索每一层留几个前缀（一个前缀算一步模型，一步约 0.6 毫秒）。
const BEAM: usize = 6;
/// 联想的词最长几个字。
const MAX_CHARS: usize = 4;
/// 模型觉得下一个字多半是这些（一句说完了、要停一下）时不联想。
const SENTENCE_END: [char; 8] = ['，', '。', '！', '？', '、', '；', '：', '…'];
/// 下一个字是标点的概率超过这个就不联想。
const MAX_END_PROB: f32 = 0.5;
/// 最有把握的词的 ln P 低于这个（不到 2%）就什么都不给。
const MIN_BEST_LOGP: f64 = -3.9;
/// 比最有把握的那个低这么多（ln，约 7 倍）的不给。
const RANGE: f64 = 2.0;
/// 最多给几个（再多就是凑数，也更容易被数字键误选）。
const MAX_SHOWN: usize = 5;
/// 以这些语气词结尾的「词」（意思啊、你啦）是词库里的半截话，不给。
const PARTICLES: [char; 4] = ['啊', '啦', '呀', '哦'];
/// 往回最多看几个字找上一个词（查输入习惯里接在它后面的词）。
const MAX_PREV_CHARS: usize = 4;

/// 词库里两到四个字、都是汉字的词，按字排好（UTF-8 字节序就是码位序），联想时按前缀找。
pub struct Words {
    /// 所有词连在一起
    text: String,
    /// 第 i 个词是 text[starts[i]..starts[i + 1]]
    starts: Vec<u32>,
    /// 这些词的头一个字（不重复）
    firsts: Vec<char>,
}

impl Words {
    pub fn build(lexicon: &dyn Lexicon) -> Self {
        let mut all: Vec<String> = Vec::new();
        lexicon.for_each_word(&mut |word, freq| {
            let n = word.chars().count();
            // 频次不到 1 的是大字表补的生僻读音
            let whole = !word.ends_with(PARTICLES);
            if freq >= 1.0 && (2..=MAX_CHARS).contains(&n) && word.chars().all(is_han) && whole {
                all.push(word.to_owned());
            }
        });
        all.sort_unstable();
        all.dedup();
        let mut text = String::with_capacity(all.iter().map(String::len).sum());
        let mut starts = Vec::with_capacity(all.len() + 1);
        let mut firsts: Vec<char> = Vec::new();
        for word in &all {
            starts.push(text.len() as u32);
            text.push_str(word);
            let first = word.chars().next().unwrap_or_default();
            if firsts.last() != Some(&first) {
                firsts.push(first);
            }
        }
        starts.push(text.len() as u32);
        Self { text, starts, firsts }
    }

    fn len(&self) -> usize {
        self.starts.len() - 1
    }

    fn word(&self, i: usize) -> &str {
        &self.text[self.starts[i] as usize..self.starts[i + 1] as usize]
    }

    /// 以 prefix 开头的词。
    fn range(&self, prefix: &str) -> Range<usize> {
        let start = partition(self.len(), |i| self.word(i) < prefix);
        let end = partition(self.len(), |i| {
            let w = self.word(i);
            w < prefix || w.starts_with(prefix)
        });
        start..end
    }

    fn contains(&self, word: &str) -> bool {
        let r = self.range(word);
        r.start < r.end && self.word(r.start) == word
    }

    /// 以 prefix 开头的更长的词里，prefix 后面那个字（不重复）。
    fn next_chars(&self, prefix: &str) -> Vec<char> {
        let mut out: Vec<char> = Vec::new();
        for i in self.range(prefix) {
            if let Some(c) = self.word(i)[prefix.len()..].chars().next() {
                if out.last() != Some(&c) {
                    out.push(c);
                }
            }
        }
        out
    }
}

/// [0, n) 里第一个让 pred 不成立的位置（pred 在前一段成立、后一段不成立）。
fn partition(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// 光标前这一句：最后一个句末标点后面的部分（模型照整句往下接；前缀稳定，模型的缓存也用得上）。
fn sentence_tail(context: &str) -> &str {
    match context.rfind(['。', '！', '？', '\n']) {
        Some(at) => &context[at + context[at..].chars().next().map_or(0, char::len_utf8)..],
        None => context,
    }
}

/// 在 words 里沿着模型往下接：(词, ln P(这个词，而且词到这里就完了))，按分数降序。
/// 模型觉得下一个字多半是标点（这句说完了）时是空的。
fn search(model: &dyn CharLm, words: &Words, context: &str) -> Vec<(String, f64)> {
    let mut chars = words.firsts.clone();
    chars.extend(SENTENCE_END);
    let first = model.next_logprobs(context, &chars);
    let (starts, ends) = first.split_at(words.firsts.len());
    if ends.iter().map(|lp| lp.exp()).sum::<f32>() > MAX_END_PROB {
        return Vec::new();
    }
    let mut beam: Vec<(String, f64)> = words
        .firsts
        .iter()
        .zip(starts)
        .map(|(c, lp)| (c.to_string(), f64::from(*lp)))
        .collect();
    let mut found: Vec<(String, f64)> = Vec::new();
    for depth in 1..=MAX_CHARS {
        keep_best(&mut beam, BEAM);
        let mut next = Vec::new();
        for (prefix, score) in &beam {
            let kids = if depth < MAX_CHARS { words.next_chars(prefix) } else { Vec::new() };
            let lps = if kids.is_empty() {
                Vec::new()
            } else {
                model.next_logprobs(&format!("{context}{prefix}"), &kids)
            };
            if depth >= 2 && words.contains(prefix) {
                // 接着往下打成更长的词的概率不算：剩下的才是「词到这里就完了」
                let more: f64 = lps.iter().map(|lp| f64::from(lp.exp())).sum();
                found.push((prefix.clone(), score + (1.0 - more).max(1e-6).ln()));
            }
            for (c, lp) in kids.iter().zip(&lps) {
                next.push((format!("{prefix}{c}"), score + f64::from(*lp)));
            }
        }
        beam = next;
    }
    found.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    found
}

/// 只留分数最高的 n 个（顺序不管）。
fn keep_best(items: &mut Vec<(String, f64)>, n: usize) {
    if items.len() > n {
        items.select_nth_unstable_by(n, |a, b| b.1.total_cmp(&a.1));
        items.truncate(n);
    }
}

impl Engine {
    /// 光标前是 context（刚上屏的字结尾）时，接下来可能打的词和分数（越大越可能），
    /// 最多 limit 个（不超过 5 个），按分数降序；没把握、没有字级模型时是空的。开着繁体时给的是繁体。
    pub fn predict(&self, context: &str, limit: usize) -> Vec<(String, f64)> {
        let context = context.trim_end();
        if limit == 0 || !context.chars().next_back().is_some_and(is_han) {
            return Vec::new();
        }
        let Some(model) = self.nnlm.read().ok().and_then(|m| m.clone()) else {
            return Vec::new();
        };
        // 第一次联想时把词库里的词按字排一遍（约 0.2 秒，只做一次；不开联想的不用花这个时间）
        let words = self.prediction_words.get_or_init(|| Words::build(&*self.base_lexicon));
        let sentence = sentence_tail(context);
        let mut found = search(&*model, words, sentence);
        // 刚打过的不再给（你好 → 你好、哈哈哈 → 哈哈）
        found.retain(|(word, _)| !context.ends_with(word.as_str()));
        let best = found.first().map_or(f64::NEG_INFINITY, |(_, s)| *s);
        found.retain(|(_, s)| best >= MIN_BEST_LOGP && *s >= best - RANGE);
        // 用户自己常接在上一个词后面的词：加分，不受上面的门槛
        if let (Some(prev), Ok(pairs)) = (self.prediction_prev(context), self.pairs.read()) {
            let now = crate::now_ms();
            for (word, entry) in pairs.followers(&prev) {
                if word == prev || !word.chars().all(is_han) {
                    continue;
                }
                let bonus = (PAIR_WEIGHT * (1.0 + decayed(entry, now)).ln()).min(PAIR_BONUS_CAP);
                match found.iter_mut().find(|(w, _)| w == word) {
                    Some(item) => item.1 += bonus,
                    None => found.push((word.to_owned(), model.score_text(sentence, word) + bonus)),
                }
            }
        }
        found.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut out: Vec<(String, f64)> = Vec::new();
        for (word, score) in found {
            if out.len() >= limit.min(MAX_SHOWN) {
                break;
            }
            let shown = self.shown_text(&word);
            // 繁体转换后重了的不给
            if out.iter().any(|(w, _)| *w == shown) {
                continue;
            }
            if self.config.traditional {
                // 和 input() 一样记下繁体对应的词库写法：选了以后学的、接着联想看的都是词库里的写法
                if let Ok(mut last) = self.last_words.write() {
                    last.insert(shown.clone(), vec![word]);
                }
            }
            out.push((shown, score));
        }
        out
    }

    /// 输入习惯里的上一个词：会话选词上屏时引擎记下的最后一个词（光标前正好是它）；不是选词上屏的，
    /// 取光标前最长的、语言模型认识的两到四个字。
    fn prediction_prev(&self, context: &str) -> Option<String> {
        let last = self.last_word.read().ok()?.clone();
        if !last.is_empty() && context.ends_with(&self.shown_text(&last)) {
            return Some(last);
        }
        let chars: Vec<char> = context.chars().collect();
        (2..=MAX_PREV_CHARS.min(chars.len()))
            .rev()
            .map(|n| chars[chars.len() - n..].iter().collect::<String>())
            .find(|word| self.lm.knows_word(word) != Some(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryLexicon;

    /// 假的字级模型：上文（这一句）以某段字结尾时，下一个字是谁、多大概率；没写到的字都很不可能。
    pub(crate) struct FakeLm(pub Vec<(&'static str, Vec<(char, f32)>)>);

    impl CharLm for FakeLm {
        fn score_text(&self, context: &str, text: &str) -> f64 {
            let mut ctx = context.to_owned();
            let mut total = 0.0;
            for c in text.chars() {
                total += f64::from(self.next_logprobs(&ctx, &[c])[0]);
                ctx.push(c);
            }
            total
        }

        fn next_logprobs(&self, context: &str, chars: &[char]) -> Vec<f32> {
            let rule = self.0.iter().filter(|(tail, _)| context.ends_with(tail)).max_by_key(|(t, _)| t.len());
            chars
                .iter()
                .map(|c| {
                    rule.and_then(|(_, next)| next.iter().find(|(n, _)| n == c))
                        .map_or(-12.0, |(_, p)| p.ln())
                })
                .collect()
        }
    }

    fn words(tsv: &str) -> Words {
        Words::build(&MemoryLexicon::from_tsv_str(tsv).unwrap())
    }

    #[test]
    fn words_by_prefix() {
        let w = words("bu zhi\t不知\t10\nbu zhi dao\t不知道\t10\nbu\t不\t10\nyi qi\t一起\t10\nxx\tabc\t10\n");
        assert_eq!(w.firsts, ['一', '不'], "单字、不是汉字的不算");
        assert!(w.contains("不知") && w.contains("不知道") && !w.contains("不"));
        assert_eq!(w.next_chars("不"), ['知']);
        assert_eq!(w.next_chars("不知"), ['道']);
        assert!(w.next_chars("不知道").is_empty());
    }

    #[test]
    fn search_follows_words_and_word_ends() {
        let w = words("tian qi\t天气\t10\nwan shang\t晚上\t10\nbu zhi\t不知\t10\nbu zhi dao\t不知道\t10\n");
        // 今天后面：天气多半、晚上其次；不知后面几乎总是道（所以「不知」排在「不知道」后面）
        let lm = FakeLm(vec![
            ("今天", vec![('天', 0.5), ('晚', 0.3)]),
            ("今天天", vec![('气', 0.9)]),
            ("今天晚", vec![('上', 0.9)]),
            ("我", vec![('不', 0.6)]),
            ("我不", vec![('知', 0.8)]),
            ("我不知", vec![('道', 0.9)]),
        ]);
        let got: Vec<String> = search(&lm, &w, "今天").into_iter().map(|(w, _)| w).collect();
        assert_eq!(&got[..2], ["天气", "晚上"]);
        let got: Vec<String> = search(&lm, &w, "我").into_iter().map(|(w, _)| w).collect();
        assert_eq!(&got[..2], ["不知道", "不知"]);
        // 模型觉得这里该打标点了：不联想
        let end = FakeLm(vec![("吗", vec![('？', 0.8), ('天', 0.1)])]);
        assert!(search(&end, &w, "你好吗").is_empty());
    }

    #[test]
    fn sentence_tail_starts_after_sentence_end() {
        assert_eq!(sentence_tail("好的。我们明天"), "我们明天");
        assert_eq!(sentence_tail("我们明天"), "我们明天");
        assert_eq!(sentence_tail("好吧！"), "");
    }
}
