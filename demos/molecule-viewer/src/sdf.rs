//! MOL / SDF V2000 parser → [`Molecule`].
//!
//! `PubChem` returns 3D structures as an SDF (a MOL block followed by
//! `M  END` and optional data items). We only need the MOL block's
//! atom + bond tables. V2000 format:
//!
//! ```text
//! <title line>
//! <program/timestamp line>
//! <comment line>
//! aaabbb...  counts line: atoms = cols 0..3, bonds = cols 3..6 (each %3d)
//! <atom block, `atoms` lines>:  x (%10.4f) y (%10.4f) z (%10.4f) <space> symbol ...
//! <bond block, `bonds` lines>:  a1 (%3d) a2 (%3d) type (%3d) ...   (1-based atom indices)
//! M  END
//! ```
//!
//! Notes:
//! - Atom indices in the bond block are **1-based**; convert to 0-based.
//! - Coordinates are fixed-width but splitting on whitespace is fine in
//!   practice for `PubChem` output; prefer robust whitespace splitting,
//!   and fall back to column slicing only if needed.
//! - Ignore everything after `M  END` (data items, `$$$$`).
//! - Be tolerant: some records are V2000, the header counts line is the
//!   4th line (index 3).

use crate::model::{Atom, Bond, Molecule};

/// Parse the first molecule from an SDF / MOL V2000 string.
///
/// Returns an error if the counts line is missing/unparseable or the
/// atom/bond tables are truncated.
pub fn parse_sdf(text: &str) -> anyhow::Result<Molecule> {
    let lines: Vec<&str> = text.lines().collect();

    // The counts line is nominally line index 3 (0-based) in V2000 format:
    //   line 0: molecule name
    //   line 1: program/timestamp
    //   line 2: comment
    //   line 3: counts line
    //
    // However some generators insert extra blank lines. We try index 3 first,
    // and if it's blank or empty we search forward for the first non-blank line
    // after index 3 (up to a reasonable limit).
    if lines.len() < 4 {
        anyhow::bail!("SDF input too short: missing counts line");
    }

    // Find the counts line index: start at 3, skip blanks.
    let counts_idx = {
        let mut idx = 3;
        while idx < lines.len() && lines[idx].trim().is_empty() {
            idx += 1;
        }
        if idx >= lines.len() {
            anyhow::bail!("SDF input has no counts line (all blank after header)");
        }
        idx
    };

    let counts_line = lines[counts_idx];

    // Parse atom/bond counts. V2000 uses fixed 3-wide columns; parse by
    // column (correct even when both counts are >=100 and merge with no
    // separating space, e.g. `100100`), with whitespace splitting kept
    // only as a fallback for non-conforming generators.
    let (n_atoms, n_bonds) = parse_counts(counts_line)?;

    // Atom block starts immediately after the counts line.
    let atom_start = counts_idx + 1;
    let atom_end = atom_start + n_atoms;
    if lines.len() < atom_end {
        anyhow::bail!(
            "SDF atom table truncated: expected {} atoms but only {} lines available after header",
            n_atoms,
            lines.len().saturating_sub(atom_start)
        );
    }

    let mut atoms = Vec::with_capacity(n_atoms);
    for (i, line) in lines.iter().enumerate().skip(atom_start).take(n_atoms) {
        let atom = parse_atom_line(line)
            .map_err(|e| anyhow::anyhow!("Error parsing atom line {}: {}: {:?}", i + 1, e, line))?;
        atoms.push(atom);
    }

    // Bond block follows atom block.
    let bond_start = atom_end;
    let bond_end = bond_start + n_bonds;
    if lines.len() < bond_end {
        anyhow::bail!(
            "SDF bond table truncated: expected {} bonds but only {} lines available",
            n_bonds,
            lines.len().saturating_sub(bond_start)
        );
    }

    let mut bonds = Vec::with_capacity(n_bonds);
    for (i, line) in lines.iter().enumerate().skip(bond_start).take(n_bonds) {
        // Stop early if we hit M  END before expected (defensive).
        if line.trim_start().starts_with("M  END") {
            break;
        }
        let bond = parse_bond_line(line)
            .map_err(|e| anyhow::anyhow!("Error parsing bond line {}: {}: {:?}", i + 1, e, line))?;
        // Validate indices against the atom count — an out-of-range bond
        // would otherwise panic downstream at `mol.atoms[bond.a]`.
        if bond.a >= n_atoms || bond.b >= n_atoms {
            anyhow::bail!(
                "bond on line {} references atom out of range (have {} atoms, got a={}, b={})",
                i + 1,
                n_atoms,
                bond.a + 1,
                bond.b + 1
            );
        }
        bonds.push(bond);
    }

    Ok(Molecule { atoms, bonds })
}

/// Parse the counts line, returning (`n_atoms`, `n_bonds`).
///
/// V2000 packs the counts as fixed 3-wide fields: atoms in columns
/// 0..3, bonds in 3..6. Parse by column FIRST — whitespace splitting
/// breaks when both counts are >=100 (e.g. `100100` has no separating
/// space and splits into a single 10-thousand-ish token). Whitespace
/// splitting is kept as a fallback for non-conforming generators.
fn parse_counts(line: &str) -> anyhow::Result<(usize, usize)> {
    // Fixed-column parse (correct V2000; `.get` avoids a panic on a
    // non-char-boundary should the line contain unexpected bytes).
    if let (Some(a), Some(b)) = (line.get(0..3), line.get(3..6))
        && let (Ok(n_atoms), Ok(n_bonds)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>())
    {
        return Ok((n_atoms, n_bonds));
    }

    // Fallback: whitespace split (handles oddly-indented non-standard
    // counts lines, at the cost of the >=100/>=100 merge case above).
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 {
        let n_atoms: usize = parts[0]
            .parse()
            .map_err(|_| anyhow::anyhow!("Cannot parse atom count {:?}", parts[0]))?;
        let n_bonds: usize = parts[1]
            .parse()
            .map_err(|_| anyhow::anyhow!("Cannot parse bond count {:?}", parts[1]))?;
        return Ok((n_atoms, n_bonds));
    }

    anyhow::bail!("Counts line too short or unparseable: {line:?}")
}

/// Parse one atom line: x y z symbol ...
fn parse_atom_line(line: &str) -> anyhow::Result<Atom> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 4 {
        anyhow::bail!("atom line has fewer than 4 fields");
    }
    let x: f32 = parts[0]
        .parse()
        .map_err(|_| anyhow::anyhow!("bad x coord {:?}", parts[0]))?;
    let y: f32 = parts[1]
        .parse()
        .map_err(|_| anyhow::anyhow!("bad y coord {:?}", parts[1]))?;
    let z: f32 = parts[2]
        .parse()
        .map_err(|_| anyhow::anyhow!("bad z coord {:?}", parts[2]))?;
    let symbol = parts[3].to_string();
    Ok(Atom {
        symbol,
        pos: [x, y, z],
    })
}

/// Parse one bond line: a1 a2 type ... (1-based atom indices → 0-based).
///
/// V2000 packs the bond block as fixed 3-wide integer fields (a1 = cols
/// 0..3, a2 = 3..6, type = 6..9). Parse by column FIRST — whitespace
/// splitting merges adjacent fields once atom indices reach 3 digits:
/// a bond between atoms 17 and 122 prints as `" 17122  1"`, which splits
/// into the bogus token `17122`. This is the same fixed-column hazard
/// `parse_counts` handles, and it only bites molecules with >=100 atoms.
/// Whitespace splitting is kept as a fallback for non-conforming output.
fn parse_bond_line(line: &str) -> anyhow::Result<Bond> {
    // Fixed-column parse: only accepted if all three 3-wide fields are
    // present and parse cleanly (a space inside a field — i.e. a
    // non-column-aligned line — makes a field fail and falls through).
    let fixed = || -> Option<(usize, usize, u8)> {
        let a1 = line.get(0..3)?.trim().parse::<usize>().ok()?;
        let a2 = line.get(3..6)?.trim().parse::<usize>().ok()?;
        let ty = line.get(6..9)?.trim().parse::<u8>().ok()?;
        Some((a1, a2, ty))
    };

    let (a1, a2, bond_type) = if let Some(t) = fixed() {
        t
    } else {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            anyhow::bail!("bond line has fewer than 3 fields");
        }
        let a1 = parts[0]
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!("bad atom index a1 {:?}", parts[0]))?;
        let a2 = parts[1]
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!("bad atom index a2 {:?}", parts[1]))?;
        let ty = parts[2]
            .parse::<u8>()
            .map_err(|_| anyhow::anyhow!("bad bond type {:?}", parts[2]))?;
        (a1, a2, ty)
    };

    if a1 == 0 || a2 == 0 {
        anyhow::bail!("bond atom indices must be 1-based (got a1={a1}, a2={a2})");
    }

    Ok(Bond {
        a: a1 - 1,
        b: a2 - 1,
        order: bond_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WATER_SDF: &str = "\
water

  test  3D

  3  2  0  0  0  0  0  0  0  0999 V2000
    0.0000    0.0000    0.0000 O   0  0  0  0  0  0  0  0  0  0  0  0
    0.7570    0.5860    0.0000 H   0  0  0  0  0  0  0  0  0  0  0  0
   -0.7570    0.5860    0.0000 H   0  0  0  0  0  0  0  0  0  0  0  0
  1  2  1  0  0  0  0
  1  3  1  0  0  0  0
M  END
";

    #[test]
    fn test_water_atom_count() {
        let mol = parse_sdf(WATER_SDF).expect("parse_sdf failed");
        assert_eq!(mol.atoms.len(), 3);
    }

    #[test]
    fn test_water_atom_symbols() {
        let mol = parse_sdf(WATER_SDF).expect("parse_sdf failed");
        assert_eq!(mol.atoms[0].symbol, "O");
        assert_eq!(mol.atoms[1].symbol, "H");
        assert_eq!(mol.atoms[2].symbol, "H");
    }

    #[test]
    fn test_water_atom_positions() {
        let mol = parse_sdf(WATER_SDF).expect("parse_sdf failed");
        let pos = mol.atoms[0].pos;
        assert!((pos[0] - 0.0).abs() < 1e-4, "x={}", pos[0]);
        assert!((pos[1] - 0.0).abs() < 1e-4, "y={}", pos[1]);
        assert!((pos[2] - 0.0).abs() < 1e-4, "z={}", pos[2]);
    }

    #[test]
    fn test_water_bond_count() {
        let mol = parse_sdf(WATER_SDF).expect("parse_sdf failed");
        assert_eq!(mol.bonds.len(), 2);
    }

    #[test]
    fn test_water_bond_order() {
        let mol = parse_sdf(WATER_SDF).expect("parse_sdf failed");
        assert_eq!(mol.bonds[0].order, 1);
        assert_eq!(mol.bonds[1].order, 1);
    }

    #[test]
    fn test_water_bond_indices() {
        let mol = parse_sdf(WATER_SDF).expect("parse_sdf failed");
        // 1-based (1,2) → 0-based (0,1)
        assert_eq!(mol.bonds[0].a, 0);
        assert_eq!(mol.bonds[0].b, 1);
        // 1-based (1,3) → 0-based (0,2)
        assert_eq!(mol.bonds[1].a, 0);
        assert_eq!(mol.bonds[1].b, 2);
    }

    #[test]
    fn test_truncated_atom_table_errors() {
        let bad = "\
mol

  test  3D

  3  2  0  0  0  0  0  0  0  0999 V2000
    0.0000    0.0000    0.0000 O   0  0
M  END
";
        let result = parse_sdf(bad);
        assert!(result.is_err(), "expected error for truncated atom table");
    }

    #[test]
    fn test_missing_counts_line_errors() {
        let bad = "too\nshort\n";
        let result = parse_sdf(bad);
        assert!(result.is_err(), "expected error for missing counts line");
    }

    #[test]
    fn test_bond_index_out_of_range_errors() {
        // 3 atoms but a bond references atom 5 → must error, not panic
        // later in geometry at `mol.atoms[bond.a]`.
        let bad = "\
mol

  test  3D

  3  1  0  0  0  0  0  0  0  0999 V2000
    0.0000    0.0000    0.0000 O   0  0  0  0  0  0  0  0  0  0  0  0
    0.7570    0.5860    0.0000 H   0  0  0  0  0  0  0  0  0  0  0  0
   -0.7570    0.5860    0.0000 H   0  0  0  0  0  0  0  0  0  0  0  0
  1  5  1  0  0  0  0
M  END
";
        let result = parse_sdf(bad);
        assert!(
            result.is_err(),
            "expected error for out-of-range bond index"
        );
    }

    #[test]
    fn test_counts_fixed_column_handles_ge_100() {
        // Both counts >= 100 merge with no separating space in V2000
        // (`100100`). Fixed-column parsing must still read 100 and 100;
        // a whitespace split would wrongly yield a single 100100 token.
        let (a, b) = parse_counts("100100  0  0  0  0  0  0  0  0999 V2000").expect("counts parse");
        assert_eq!((a, b), (100, 100));
    }

    #[test]
    fn test_counts_standard_small() {
        let (a, b) = parse_counts("  3  2  0  0  0  0  0  0  0  0999 V2000").expect("counts parse");
        assert_eq!((a, b), (3, 2));
    }

    #[test]
    fn test_bond_line_fixed_columns_ge_100() {
        // Atoms 17 → 122 in V2000 fixed 3-wide columns: " 17" "122" "  1".
        // Whitespace splitting would merge " 17"+"122" into the bogus
        // token 17122 (the CID 75534892 dendrimer bug).
        let bond = parse_bond_line(" 17122  1  0  0  0  0").expect("bond parse");
        assert_eq!((bond.a, bond.b, bond.order), (16, 121, 1));
    }

    #[test]
    fn test_bond_line_small_columns() {
        let bond = parse_bond_line("  1  2  1  0  0  0  0").expect("bond parse");
        assert_eq!((bond.a, bond.b, bond.order), (0, 1, 1));
    }

    #[test]
    fn test_bond_line_nonaligned_fallback() {
        // Single-space-separated (non-standard) → fixed columns fail, the
        // whitespace fallback still parses it.
        let bond = parse_bond_line("10 20 1").expect("bond parse");
        assert_eq!((bond.a, bond.b, bond.order), (9, 19, 1));
    }
}
