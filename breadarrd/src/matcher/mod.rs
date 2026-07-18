pub mod embed;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use embed::{cosine_similarity, OrtEmbedder};

const MODEL_URL: &str =
    "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/onnx/model.onnx";
const TOKENIZER_URL: &str =
    "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/tokenizer.json";

/// Downloads the embedding model into `model_dir` if it isn't already
/// there — keeps setup to "run the daemon," no separate fetch step, in
/// keeping with the project's minimal-setup goal.
///
/// `bread_onnx::download::ensure_file` is sync/blocking (`ureq`, matching
/// this workspace's `bakery` download convention) — this used to be a
/// `reqwest`-based async implementation local to this crate, genuinely
/// duplicating breadmill's own sync/`ureq` downloader. Since this fn is
/// called from an async context, each call is dispatched via
/// `spawn_blocking` rather than blocking the async runtime directly.
pub async fn ensure_model(model_dir: &Path) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(model_dir)
        .with_context(|| format!("failed to create {}", model_dir.display()))?;

    let model_path = model_dir.join("model.onnx");
    let tokenizer_path = model_dir.join("tokenizer.json");

    if !model_path.exists() {
        tracing::info!("downloading title-matching model (~90MB, one-time)");
        download(MODEL_URL, model_path.clone()).await?;
    }
    if !tokenizer_path.exists() {
        download(TOKENIZER_URL, tokenizer_path.clone()).await?;
    }

    Ok((model_path, tokenizer_path))
}

async fn download(url: &'static str, dest: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || bread_onnx::download::ensure_file(url, &dest, None))
        .await
        .context("download task panicked")??;
    Ok(())
}

const EMBEDDING_DIM: usize = 384;

/// Below this cosine similarity, no match is proposed at all.
pub(crate) const MIN_CONFIDENCE: f32 = 0.55;
/// At or above this, auto-match without queuing for manual review — but see
/// `token_overlap_ratio` below, which gates this further.
const AUTO_MATCH_CONFIDENCE: f32 = 0.85;
/// Auto-match additionally requires at least this much literal word overlap
/// between the query and the matched title/alias. all-MiniLM-L6-v2 is
/// trained on English semantic similarity; on romanized Japanese anime
/// titles it doesn't actually discriminate between shows — it clusters
/// *any* two romaji titles close together (shared particles/phonetics read
/// as "similar foreign text"), producing confidently wrong matches between
/// completely unrelated series (verified live: multiple unrelated
/// currently-airing anime auto-matched to "Demon Slayer" and "Mushoku
/// Tensei" at >0.85 confidence with zero actual relation). Requiring some
/// real word overlap catches exactly this failure mode — a genuine match
/// (including a registered alias) shares at least one real token; a
/// same-cluster false positive usually shares none.
const MIN_TOKEN_OVERLAP: f32 = 0.2;

#[derive(Debug, Clone, PartialEq)]
pub struct MatchCandidate {
    pub media_item_id: i64,
    pub matched_text: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MatchOutcome {
    Auto(MatchCandidate),
    NeedsReview(MatchCandidate),
    NoMatch,
}

pub struct TitleMatcher {
    embedder: OrtEmbedder,
    cache: HashMap<String, Vec<f32>>,
}

impl TitleMatcher {
    pub fn load(model_path: &Path, tokenizer_path: &Path) -> Result<Self> {
        Ok(Self {
            embedder: OrtEmbedder::load(model_path, tokenizer_path, EMBEDDING_DIM)?,
            cache: HashMap::new(),
        })
    }

    fn embed_cached(&mut self, text: &str) -> Result<Vec<f32>> {
        if let Some(v) = self.cache.get(text) {
            return Ok(v.clone());
        }
        let v = self.embedder.embed(text)?;
        self.cache.insert(text.to_string(), v.clone());
        Ok(v)
    }

    /// Given a flat list of candidate texts (e.g. every search result's
    /// name plus its aliases, flattened with an index back to which result
    /// each one belongs to), returns the index of whichever candidate text
    /// best matches `query` and its similarity score. Generic version of
    /// the logic `match_title` uses against the DB — also used to
    /// disambiguate metadata-provider search results, where the top result
    /// isn't always the right entry (TVDB's own relevance ranking can
    /// place a spinoff/short above the main series, e.g. searching "Attack
    /// on Titan" ranks "Attack on Titan: Counter Rockets" first even
    /// though the real series' own alias list has "Attack on Titan
    /// (2013)", which should score far closer to the query).
    pub fn best_match_index(
        &mut self,
        query: &str,
        candidates: &[(usize, String)],
    ) -> Result<Option<(usize, f32)>> {
        let query_emb = self.embed_cached(query)?;
        let mut best: Option<(usize, f32)> = None;
        for (owner_index, text) in candidates {
            let emb = self.embed_cached(text)?;
            let score = cosine_similarity(&query_emb, &emb);
            if best.as_ref().is_none_or(|(_, s)| score > *s) {
                best = Some((*owner_index, score));
            }
        }
        Ok(best)
    }

    /// Matches `query` against every monitored media_item's title + known
    /// aliases, returning the single best match and whether it clears the
    /// auto-match bar or needs a human to confirm it in the review queue.
    pub fn match_title(&mut self, conn: &Connection, query: &str) -> Result<MatchOutcome> {
        let query_emb = self.embed_cached(query)?;

        let mut stmt = conn.prepare(
            "SELECT id, title FROM media_item WHERE monitored = 1
             UNION ALL
             SELECT a.media_item_id, a.text FROM alias a
             JOIN media_item m ON m.id = a.media_item_id WHERE m.monitored = 1",
        )?;
        let candidates: Vec<(i64, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut best: Option<MatchCandidate> = None;
        for (media_item_id, text) in candidates {
            let emb = self.embed_cached(&text)?;
            let confidence = cosine_similarity(&query_emb, &emb);
            if confidence < MIN_CONFIDENCE {
                continue;
            }
            if best.as_ref().is_none_or(|b| confidence > b.confidence) {
                best = Some(MatchCandidate {
                    media_item_id,
                    matched_text: text,
                    confidence,
                });
            }
        }

        Ok(match best {
            None => MatchOutcome::NoMatch,
            // The embedding-clustering failure mode this guards against
            // (see this struct's field docs / `MIN_TOKEN_OVERLAP`) lands
            // its false positives anywhere in [MIN_CONFIDENCE,
            // AUTO_MATCH_CONFIDENCE) just as often as above it — a
            // completely unrelated romaji title scores confidently
            // *enough* to pass MIN_CONFIDENCE, just not confidently enough
            // to auto-match. Gating only the auto-match branch left every
            // one of those false positives landing in the review queue
            // instead, silently flooding it (verified live: dozens of
            // zero-overlap titles queued against a handful of "attractor"
            // shows). Zero real word overlap means "not a candidate at
            // all," not "uncertain, ask a human."
            Some(c) if token_overlap_ratio(query, &c.matched_text) < MIN_TOKEN_OVERLAP => {
                MatchOutcome::NoMatch
            }
            Some(c) if c.confidence >= AUTO_MATCH_CONFIDENCE => MatchOutcome::Auto(c),
            Some(c) => MatchOutcome::NeedsReview(c),
        })
    }
}

/// Grammatical filler words that show up across countless unrelated titles
/// and carry no discriminating signal on their own — English articles and
/// the short romaji particles ubiquitous in Japanese titles ("X no Y", "X ga
/// Y", ...). Without this, two completely unrelated shows sharing nothing
/// but e.g. "no" register as real overlap and slip past `MIN_TOKEN_OVERLAP`
/// (verified live: "Yomi no Tsugai" / "Kami no Shizuku" / "Saijo no Osewa"
/// all cleared the guard against "Kimetsu no Yaiba" on "no" alone).
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "of", "to", "and", "no", "wa", "wo", "ga", "ni", "de", "ha", "he", "da",
];

/// Word-level overlap between `a` and `b`, as a fraction of the *shorter*
/// title's word count — so a short alias fully contained in a longer title
/// (or vice versa) still scores 1.0, rather than being penalized by length
/// mismatch. Case-insensitive, splits on non-alphanumeric runs, and ignores
/// `STOPWORDS` so shared filler words don't count as real overlap.
fn token_overlap_ratio(a: &str, b: &str) -> f32 {
    let tokenize = |s: &str| -> std::collections::HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_lowercase())
            .filter(|t| !STOPWORDS.contains(&t.as_str()))
            .collect()
    };
    let ta = tokenize(a);
    let tb = tokenize(b);
    let shorter = ta.len().min(tb.len());
    if shorter == 0 {
        return 0.0;
    }
    let overlap = ta.intersection(&tb).count();
    overlap as f32 / shorter as f32
}

/// `link`/`source_id` are stored (not just the title) so an approval can
/// later actually complete the grab, rather than just recording a decision.
/// Both `None` when there's no real source item behind the query (e.g. an
/// ad hoc matcher test) — such an entry can be reviewed but not approved.
pub fn queue_for_review(
    conn: &Connection,
    raw_release_title: &str,
    candidate: &MatchCandidate,
    link: Option<&str>,
    source_id: Option<i64>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO review_queue (raw_release_title, candidate_media_item_id, confidence, link, source_id, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', datetime('now'))",
        params![raw_release_title, candidate.media_item_id, candidate.confidence, link, source_id],
    )?;
    crate::db::record_event(
        conn,
        candidate.media_item_id,
        None,
        "review_queued",
        &format!(
            "confidence={:.2} title={raw_release_title:?}",
            candidate.confidence
        ),
    )?;
    Ok(conn.last_insert_rowid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_titles_fully_overlap() {
        assert_eq!(token_overlap_ratio("Mushoku Tensei", "Mushoku Tensei"), 1.0);
    }

    #[test]
    fn short_alias_contained_in_longer_title_fully_overlaps() {
        // The alias is fully covered by the longer title's words, so this
        // should score 1.0 despite the length difference — an alias like
        // "Mushoku Tensei" shouldn't be penalized against the full title
        // "Mushoku Tensei: Jobless Reincarnation".
        let ratio = token_overlap_ratio("Mushoku Tensei", "Mushoku Tensei Jobless Reincarnation");
        assert!((ratio - 1.0).abs() < 1e-6, "ratio was {ratio}");
    }

    #[test]
    fn unrelated_titles_have_no_overlap() {
        // The real failure case this guards against: two unrelated
        // romanized Japanese anime titles that an English-trained embedding
        // model scores as deceptively similar.
        let ratio = token_overlap_ratio(
            "Sekai Saikyou no Kouei",
            "Kimi wo Aisuru Ki wa Nai to Itta Jiki Koushaku-sama ga Nazeka Dekiai Shitekimasu",
        );
        assert_eq!(ratio, 0.0);
    }

    #[test]
    fn empty_input_has_zero_overlap() {
        assert_eq!(token_overlap_ratio("", "Mushoku Tensei"), 0.0);
    }

    #[test]
    fn shared_particle_alone_is_not_real_overlap() {
        // The bug found live in the review queue: unrelated titles sharing
        // only the Japanese particle "no" scored a nonzero ratio and slipped
        // past MIN_TOKEN_OVERLAP, flooding review with false matches against
        // "Demon Slayer" (Kimetsu no Yaiba).
        assert_eq!(
            token_overlap_ratio("Yomi no Tsugai", "Kimetsu no Yaiba"),
            0.0
        );
        assert_eq!(
            token_overlap_ratio("Kami no Shizuku", "Kimetsu no Yaiba"),
            0.0
        );
        assert_eq!(
            token_overlap_ratio("Saijo no Osewa", "Kimetsu no Yaiba"),
            0.0
        );
    }
}
