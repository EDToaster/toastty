//! `PubChem` PUG-REST client (blocking, via `ureq` 3.x).
//!
//! Base URL: `https://pubchem.ncbi.nlm.nih.gov/rest/pug`.
//!
//! Confirmed-live endpoint shapes:
//! - Formula search (synchronous):
//!   `GET /compound/fastformula/{formula}/cids/JSON`
//!   → `{"IdentifierList":{"CID":[702, 8254, ...]}}`
//! - Name search:
//!   `GET /compound/name/{name}/cids/JSON` (same shape)
//! - Properties (batch, comma-separated CIDs):
//!   `GET /compound/cid/{a,b,c}/property/Title,IUPACName,MolecularFormula/JSON`
//!   → `{"PropertyTable":{"Properties":[{"CID":702,"MolecularFormula":"C2H6O","IUPACName":"ethanol","Title":"Ethanol"}, ...]}}`
//! - 3D structure:
//!   `GET /compound/cid/{cid}/record/SDF?record_type=3d`  (falls back to `2d`)
//!
//! `ureq` 3.x usage sketch (verify against the resolved 3.3 API while
//! implementing): `ureq::get(url).call()?.body_mut().read_to_string()?`.
//! A 404 (e.g. no 3D conformer) surfaces as an error status — catch it
//! to drive the 2d fallback.

const BASE: &str = "https://pubchem.ncbi.nlm.nih.gov/rest/pug";

/// A candidate compound for the disambiguation list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub cid: u32,
    /// Common name, e.g. `"Ethanol"` (`PubChem` `Title`).
    pub title: String,
    /// IUPAC name, e.g. `"ethanol"`.
    pub iupac: String,
    /// Molecular formula, e.g. `"C2H6O"`.
    pub formula: String,
}

/// Returns `true` if `query` looks like a molecular formula, e.g. `"C2H6O"`.
///
/// Heuristic: the string must consist of one or more groups of an uppercase
/// letter followed by an optional lowercase letter and optional digits.
fn is_formula(query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        // Must start each group with an uppercase letter.
        if !c.is_ascii_uppercase() {
            return false;
        }
        // Optional lowercase letter.
        if chars.peek().is_some_and(char::is_ascii_lowercase) {
            chars.next();
        }
        // Optional digits.
        while chars.peek().is_some_and(char::is_ascii_digit) {
            chars.next();
        }
    }
    true
}

/// URL-percent-encode a query string for embedding in a path segment.
///
/// Only encodes characters that are not unreserved (A-Z, a-z, 0-9, `-`, `_`,
/// `.`, `~`). Spaces become `%20`.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(char::from_digit((b >> 4) as u32, 16).unwrap().to_ascii_uppercase());
                out.push(char::from_digit((b & 0xf) as u32, 16).unwrap().to_ascii_uppercase());
            }
        }
    }
    out
}

/// Parse `{"IdentifierList":{"CID":[...]}}` and return the CID list.
fn parse_cid_list(json: &str) -> anyhow::Result<Vec<u32>> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    let arr = v
        .get("IdentifierList")
        .and_then(|il| il.get("CID"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow::anyhow!("unexpected CID response shape"))?;
    let cids = arr
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .map(|x| x as u32)
        .collect();
    Ok(cids)
}

/// Parse `{"PropertyTable":{"Properties":[...]}}` into `Candidate` list.
fn parse_properties(json: &str) -> anyhow::Result<Vec<Candidate>> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    let arr = v
        .get("PropertyTable")
        .and_then(|pt| pt.get("Properties"))
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow::anyhow!("unexpected Properties response shape"))?;

    let candidates = arr
        .iter()
        .filter_map(|item| {
            let cid = item.get("CID")?.as_u64()? as u32;
            let title = item
                .get("Title")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let iupac = item
                .get("IUPACName")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let formula = item
                .get("MolecularFormula")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            Some(Candidate {
                cid,
                title,
                iupac,
                formula,
            })
        })
        .collect();
    Ok(candidates)
}

/// Parse a direct-CID query: a bare positive integer, optionally prefixed
/// with a case-insensitive `cid` and a `:`/whitespace separator (`702`,
/// `cid 702`, `CID:702`, `cid702`). Returns `None` for anything else —
/// including names that merely start with "cid" such as `cidofovir`.
fn parse_cid(query: &str) -> Option<u32> {
    let q = query.trim();
    // Strip an optional case-insensitive "cid" prefix + separator.
    let digits = match q.get(..3) {
        Some(p) if p.eq_ignore_ascii_case("cid") => q[3..].trim_start_matches([':', ' ', '\t']),
        _ => q,
    };
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        digits.parse::<u32>().ok()
    } else {
        None
    }
}

/// Look up a single compound directly by CID, returned as a one-element
/// candidate list so the caller's single-result path renders it without a
/// disambiguation step. An unknown CID (404) yields an empty list.
fn lookup_cid(cid: u32) -> anyhow::Result<Vec<Candidate>> {
    let url = format!("{BASE}/compound/cid/{cid}/property/Title,IUPACName,MolecularFormula/JSON");
    match ureq::get(&url).call() {
        Ok(mut resp) => {
            let json = resp.body_mut().read_to_string()?;
            parse_properties(&json)
        }
        // Unknown CID → 404; treat as "no results" rather than a hard error.
        Err(ureq::Error::StatusCode(_)) => Ok(vec![]),
        Err(e) => Err(e.into()),
    }
}

/// Classify `query` and search `PubChem`, returning ranked, de-duplicated
/// candidates (cap ~8). Blocking network call. A bare integer or `cid:<n>`
/// is a direct CID lookup (returns that one compound, skipping the
/// disambiguation step).
///
/// Classification heuristic: a formula matches roughly
/// `^([A-Z][a-z]?\d*)+$` (element-symbol + optional count, repeated);
/// otherwise treat as a name. Formula path uses `fastformula`; name
/// path uses the name endpoint.
///
/// Ranking/dedup: fetch properties for the first ~20 CIDs in one batch
/// request; for formula queries, keep only candidates whose
/// `MolecularFormula` exactly equals the query (drops isotopologues /
/// charged variants); de-duplicate by `Title`; sort by ascending CID
/// (lower CID ≈ more canonical); cap at 8.
pub fn search(query: &str) -> anyhow::Result<Vec<Candidate>> {
    // Direct CID lookup: a bare integer or `cid:<n>` skips search entirely.
    if let Some(cid) = parse_cid(query) {
        return lookup_cid(cid);
    }

    let encoded = url_encode(query);
    let cid_url = if is_formula(query) {
        format!("{BASE}/compound/fastformula/{encoded}/cids/JSON")
    } else {
        format!("{BASE}/compound/name/{encoded}/cids/JSON")
    };

    let cid_json = ureq::get(&cid_url)
        .call()?
        .body_mut()
        .read_to_string()?;

    let mut cids = parse_cid_list(&cid_json)?;
    cids.truncate(20);

    if cids.is_empty() {
        return Ok(vec![]);
    }

    let cid_str: String = cids
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let prop_url = format!(
        "{BASE}/compound/cid/{cid_str}/property/Title,IUPACName,MolecularFormula/JSON"
    );

    let prop_json = ureq::get(&prop_url)
        .call()?
        .body_mut()
        .read_to_string()?;

    let mut candidates = parse_properties(&prop_json)?;

    // For formula queries, keep only exact formula matches.
    if is_formula(query) {
        candidates.retain(|c| c.formula == query);
    }

    // Sort by ascending CID (lower = more canonical).
    candidates.sort_by_key(|c| c.cid);

    // De-duplicate by Title (keep first occurrence after sorting).
    let mut seen_titles = std::collections::HashSet::new();
    candidates.retain(|c| seen_titles.insert(c.title.clone()));

    // Cap at 8.
    candidates.truncate(8);

    Ok(candidates)
}

/// Fetch a compound's 3D SDF by CID (`record_type=3d`), falling back to
/// `2d` when no 3D conformer exists (the 3d request 404s). Blocking
/// network call. Returns the raw SDF text.
pub fn fetch_sdf_3d(cid: u32) -> anyhow::Result<String> {
    let url_3d = format!("{BASE}/compound/cid/{cid}/record/SDF?record_type=3d");

    match ureq::get(&url_3d).call() {
        Ok(mut resp) => Ok(resp.body_mut().read_to_string()?),
        Err(ureq::Error::StatusCode(_)) => {
            // No 3D conformer available — fall back to 2D.
            let url_2d = format!("{BASE}/compound/cid/{cid}/record/SDF?record_type=2d");
            Ok(ureq::get(&url_2d)
                .call()?
                .body_mut()
                .read_to_string()?)
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured sample responses from the live endpoint.
    const FASTFORMULA_JSON: &str =
        r#"{"IdentifierList":{"CID":[702,8254,102138,177555036]}}"#;

    const PROPERTIES_JSON: &str = r#"{"PropertyTable":{"Properties":[{"CID":702,"MolecularFormula":"C2H6O","IUPACName":"ethanol","Title":"Ethanol"}]}}"#;

    #[test]
    fn test_parse_cid_list() {
        let cids = parse_cid_list(FASTFORMULA_JSON).expect("parse cids");
        assert_eq!(cids, vec![702u32, 8254, 102138, 177555036]);
    }

    #[test]
    fn test_parse_properties() {
        let candidates = parse_properties(PROPERTIES_JSON).expect("parse properties");
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!(c.cid, 702);
        assert_eq!(c.formula, "C2H6O");
        assert_eq!(c.iupac, "ethanol");
        assert_eq!(c.title, "Ethanol");
    }

    #[test]
    fn test_is_formula_true() {
        assert!(is_formula("C2H6O"), "C2H6O should be a formula");
        assert!(is_formula("CH4"), "CH4 should be a formula");
        assert!(is_formula("C"), "single element should be a formula");
        assert!(is_formula("NaCl"), "NaCl should be a formula");
        assert!(is_formula("C12H22O11"), "C12H22O11 should be a formula");
    }

    #[test]
    fn test_is_formula_false() {
        assert!(!is_formula("aspirin"), "aspirin should be a name");
        assert!(!is_formula("ethanol"), "ethanol should be a name");
        assert!(!is_formula(""), "empty string should not be a formula");
        assert!(!is_formula("123"), "digits-only should not be a formula");
        assert!(!is_formula("water"), "water should be a name");
    }

    #[test]
    fn test_classifier_formula() {
        assert!(is_formula("C2H6O"));
    }

    #[test]
    fn test_classifier_name() {
        assert!(!is_formula("aspirin"));
    }

    #[test]
    fn test_parse_cid() {
        assert_eq!(parse_cid("702"), Some(702));
        assert_eq!(parse_cid("  702 "), Some(702));
        assert_eq!(parse_cid("cid 702"), Some(702));
        assert_eq!(parse_cid("CID:702"), Some(702));
        assert_eq!(parse_cid("cid702"), Some(702));
        assert_eq!(parse_cid("Cid 2519"), Some(2519));
        // Not CIDs:
        assert_eq!(parse_cid("aspirin"), None);
        assert_eq!(parse_cid("C2H6O"), None);
        assert_eq!(parse_cid("cidofovir"), None, "name starting with 'cid'");
        assert_eq!(parse_cid(""), None);
        assert_eq!(parse_cid("2-propanol"), None);
    }

    #[test]
    fn test_url_encode() {
        assert_eq!(url_encode("C2H6O"), "C2H6O");
        assert_eq!(url_encode("water molecule"), "water%20molecule");
        assert_eq!(url_encode("caffeine"), "caffeine");
    }

    #[test]
    fn test_dedup_and_sort() {
        // Simulate the ranking/dedup logic with synthetic data.
        let mut candidates = vec![
            Candidate {
                cid: 100,
                title: "Alpha".into(),
                iupac: "alpha".into(),
                formula: "C2H6O".into(),
            },
            Candidate {
                cid: 50,
                title: "Beta".into(),
                iupac: "beta".into(),
                formula: "C2H6O".into(),
            },
            Candidate {
                cid: 200,
                title: "Alpha".into(), // duplicate title
                iupac: "alpha2".into(),
                formula: "C2H6O".into(),
            },
        ];

        // Sort by ascending CID.
        candidates.sort_by_key(|c| c.cid);

        // De-dup by title.
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.title.clone()));

        // After sort: [50/Beta, 100/Alpha, 200/Alpha(dup)]
        // After dedup: [50/Beta, 100/Alpha]
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].cid, 50);
        assert_eq!(candidates[1].cid, 100);
    }

    /// Live network test — skipped in offline / CI environments.
    #[test]
    #[ignore]
    fn test_search_live_formula() {
        let results = search("C2H6O").expect("live search");
        assert!(!results.is_empty());
        // All results should match the formula.
        for c in &results {
            assert_eq!(c.formula, "C2H6O");
        }
    }

    /// Live network test — skipped in offline / CI environments.
    #[test]
    #[ignore]
    fn test_search_live_name() {
        let results = search("aspirin").expect("live name search");
        assert!(!results.is_empty());
    }

    /// Live network test — skipped in offline / CI environments.
    #[test]
    #[ignore]
    fn test_fetch_sdf_3d_live() {
        // CID 702 = ethanol, should have a 3D conformer.
        let sdf = fetch_sdf_3d(702).expect("fetch sdf");
        assert!(sdf.contains("$$$$"));
    }
}
