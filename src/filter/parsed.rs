//! Parser-backed filter checks built on `smiles-parser` and
//! `molecular-formulas`.
//!
//! These are the expensive tier: they parse the SMILES once and then evaluate
//! atom count, element composition, isotopes, radicals, net charge, and mass.

use elements_rs::Element;
use molecular_formulas::{ChargedMolecularFormula, MolecularFormula, prelude::ChemicalFormula};
use smiles_parser::prelude::{AromaticityPolicy, Smiles};

use crate::config::filters::{AtomCount, ElementSet, MassKind};

/// Counts heavy atoms (non-hydrogen explicit nodes) in a parsed molecule.
pub fn heavy_atom_count(s: &Smiles) -> usize {
    s.nodes()
        .iter()
        .filter(|a| a.element() != Some(Element::H))
        .count()
}

/// Counts all atoms including implicit and explicit hydrogens.
///
/// Explicit nodes (heavy atoms plus any explicit `[H]`) are counted via
/// `nodes().len()`; implicit hydrogens are added from the per-atom implicit
/// hydrogen counts.
pub fn total_atom_count(s: &Smiles) -> usize {
    let explicit = s.nodes().len();
    let implicit: usize = s
        .implicit_hydrogen_counts()
        .iter()
        .map(|&h| h as usize)
        .sum();
    explicit + implicit
}

/// Returns the net formal charge as the sum of per-atom formal charges.
pub fn net_charge(s: &Smiles) -> i32 {
    s.nodes().iter().map(|a| i32::from(a.charge_value())).sum()
}

/// Returns `true` if the count under `mode` lies within `[min, max]`
/// (each bound optional).
pub fn atom_count_in_bounds(
    s: &Smiles,
    mode: AtomCount,
    min: Option<u32>,
    max: Option<u32>,
) -> bool {
    let count = match mode {
        AtomCount::Heavy => heavy_atom_count(s),
        AtomCount::Total => total_atom_count(s),
    } as u64;
    if let Some(lo) = min
        && count < u64::from(lo)
    {
        return false;
    }
    if let Some(hi) = max
        && count > u64::from(hi)
    {
        return false;
    }
    true
}

/// Returns `true` if every atom's element satisfies the element set.
pub fn composition_ok(s: &Smiles, set: &ElementSet) -> bool {
    match set {
        ElementSet::Whitelist(allowed) => s.nodes().iter().all(|a| match a.element() {
            Some(e) => allowed.contains(&e),
            None => false, // wildcard atom: not on any concrete whitelist
        }),
        ElementSet::Blacklist(forbidden) => s.nodes().iter().all(|a| match a.element() {
            Some(e) => !forbidden.contains(&e),
            None => true,
        }),
    }
}

/// Returns `true` if any atom carries an explicit isotope label.
pub fn has_isotope(s: &Smiles) -> bool {
    s.nodes().iter().any(|a| a.isotope_mass_number().is_some())
}

/// Returns the principal (charge-adjusted) valences considered "filled" for an
/// element. Returns `None` for elements outside the supported organic subset,
/// for which radical detection is skipped.
///
/// This is a deliberately small, best-effort table. The v1 corpus sources
/// (PubChem, ZINC20, Enamine REAL) are catalogues of stable, purchasable
/// compounds where radicals are vanishingly rare, so the table covers the
/// organic subset plus common heteroatoms and intentionally does not attempt
/// exotic hypervalency.
#[allow(
    clippy::match_same_arms,
    reason = "one explicit arm per element reads as a chemistry table; merging by value obscures it"
)]
fn standard_valences(e: Element) -> Option<&'static [u8]> {
    Some(match e {
        Element::H => &[1],
        Element::B => &[3],
        Element::C => &[4],
        Element::N => &[3],
        Element::O => &[2],
        Element::F | Element::Cl | Element::Br | Element::I => &[1],
        Element::P => &[3, 5],
        Element::S => &[2, 4, 6],
        Element::Se => &[2, 4, 6],
        Element::Si => &[4],
        _ => return None,
    })
}

/// Returns `true` if the molecule contains a detectable radical (an atom whose realized valence falls below an allowed valence, leaving unpaired electrons).
///
/// For each atom we compute the realized total valence (bond orders plus explicit and implicit hydrogens) via `smiles-parser`, adjust the element's allowed valences by the atom's formal charge, and flag the atom if its realized valence is strictly below the smallest allowed valence that is at least as large as it (that is, there is an unsatisfied valence with no exact match). Atoms whose element is outside [`standard_valences`] are never flagged.
pub fn has_radical(s: &Smiles) -> bool {
    // Use the aromaticity-aware valence so that aromatic atoms (e.g. benzene
    // carbons) report their true filled valence (4) rather than the raw sum of
    // integer bond orders (which would undercount aromatic bonds and falsely
    // flag a radical).
    let aromaticity = s.aromaticity_assignment_for(AromaticityPolicy::RdkitDefault);
    s.nodes().iter().enumerate().any(|(id, atom)| {
        let Some(element) = atom.element() else {
            return false;
        };
        let Some(valences) = standard_valences(element) else {
            return false;
        };
        let realized = i32::from(s.smarts_total_valence(id, &aromaticity));
        let charge = i32::from(atom.charge_value());
        // Allowed (charge-adjusted) valences, never negative.
        let mut exact = false;
        let mut smallest_geq: Option<i32> = None;
        for &v in valences {
            let adj = i32::from(v) + charge;
            if adj < 0 {
                continue;
            }
            if adj == realized {
                exact = true;
                break;
            }
            if adj > realized {
                smallest_geq = Some(smallest_geq.map_or(adj, |cur| cur.min(adj)));
            }
        }
        if exact {
            return false;
        }
        // Radical only when realized sits strictly below an allowed valence
        // (unsatisfied bonds). Realized above all allowed valences is treated
        // as hypervalent, not radical.
        smallest_geq.is_some()
    })
}

/// Computes the requested mass (Da) for a parsed molecule.
pub fn mass_da(s: &Smiles, kind: MassKind) -> f64 {
    let formula: ChemicalFormula<u32, i32> = ChemicalFormula::from(s);
    match kind {
        MassKind::Monoisotopic => formula.isotopologue_mass(),
        MassKind::Average => formula.molar_mass(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(smi: &str) -> Smiles {
        smi.parse().unwrap_or_else(|e| panic!("parse {smi}: {e:?}"))
    }

    #[test]
    fn counts_atoms() {
        let ethanol = parse("CCO");
        assert_eq!(heavy_atom_count(&ethanol), 3);
        // C2H6O = 9 atoms total.
        assert_eq!(total_atom_count(&ethanol), 9);
    }

    #[test]
    fn net_charge_sums() {
        assert_eq!(net_charge(&parse("CCO")), 0);
        assert_eq!(net_charge(&parse("[NH4+]")), 1);
        assert_eq!(net_charge(&parse("CC(=O)[O-]")), -1);
    }

    #[test]
    fn composition_whitelist_blacklist() {
        let chno = ElementSet::Whitelist(vec![Element::C, Element::H, Element::N, Element::O]);
        assert!(composition_ok(&parse("CCO"), &chno));
        assert!(composition_ok(&parse("CC(=O)N"), &chno));
        // Contains S, not in whitelist.
        assert!(!composition_ok(&parse("CCS"), &chno));

        let no_metals = ElementSet::Blacklist(vec![Element::Na, Element::Fe]);
        assert!(composition_ok(&parse("CCO"), &no_metals));
    }

    #[test]
    fn detects_isotope_label() {
        assert!(has_isotope(&parse("[13C]CO")));
        assert!(!has_isotope(&parse("CCO")));
    }

    #[test]
    fn detects_radicals() {
        // Stable molecules: no radical.
        assert!(!has_radical(&parse("CCO")));
        assert!(!has_radical(&parse("c1ccccc1")));
        assert!(!has_radical(&parse("CC(=O)[O-]")));
        assert!(!has_radical(&parse("[NH4+]")));
        // Explicit radicals.
        assert!(has_radical(&parse("[CH3]"))); // methyl radical, valence 3 < 4
        assert!(has_radical(&parse("[CH2]"))); // carbene, valence 2 < 4
        assert!(has_radical(&parse("[O]"))); // atomic oxygen, valence 0 < 2
    }

    #[test]
    fn mass_is_reasonable() {
        // Water monoisotopic ~18.0106, average ~18.015.
        let w = parse("O");
        let mono = mass_da(&w, MassKind::Monoisotopic);
        assert!((mono - 18.0106).abs() < 0.01, "got {mono}");
        let avg = mass_da(&w, MassKind::Average);
        assert!((avg - 18.015).abs() < 0.05, "got {avg}");
    }
}
