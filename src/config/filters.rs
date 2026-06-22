//! Per-generation filter configuration and its builder.
//!
//! A [`Filters`] value is compiled into a [`FilterEngine`](crate::filter::FilterEngine)
//! that decides which molecules enter the corpus. All fields are private; build
//! via [`Filters::builder`].

use elements_rs::Element;

use crate::{LosError, Result};

/// Whether atom-count bounds apply to heavy atoms only or all atoms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomCount {
    /// Count only non-hydrogen atoms.
    Heavy,
    /// Count all atoms including (implicit and explicit) hydrogens.
    Total,
}

/// Which notion of molecular mass the mass bounds apply to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MassKind {
    /// Monoisotopic mass (most abundant isotope of each element).
    Monoisotopic,
    /// Average (molar) mass over natural isotopic abundance.
    Average,
}

/// The set of elements a molecule is permitted to contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElementSet {
    /// Only these elements are allowed; any other rejects the molecule.
    Whitelist(Vec<Element>),
    /// These elements are forbidden; all others are allowed.
    Blacklist(Vec<Element>),
}

/// Compiled, validated per-generation filter configuration.
///
/// Construct with [`Filters::builder`]. An all-default `Filters` (no bounds, no
/// element set) still enforces the connectivity / isotope / radical / charge
/// defaults described on [`FiltersBuilder`].
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub(crate) min_mass_da: Option<f64>,
    pub(crate) max_mass_da: Option<f64>,
    pub(crate) mass_kind: Option<MassKind>,
    pub(crate) min_atoms: Option<u32>,
    pub(crate) max_atoms: Option<u32>,
    pub(crate) atom_count_mode: Option<AtomCount>,
    pub(crate) elements: Option<ElementSet>,
    pub(crate) allow_isotopes: Option<bool>,
    pub(crate) allow_radicals: Option<bool>,
    pub(crate) require_single_component: Option<bool>,
    pub(crate) max_abs_charge: Option<u32>,
}

impl Filters {
    /// Starts a new [`FiltersBuilder`].
    pub fn builder() -> FiltersBuilder {
        FiltersBuilder::default()
    }

    // Accessors with the effective defaults applied. These are what the filter
    // engine reads, so the defaults documented on the builder live here.

    pub(crate) fn mass_kind_or_default(&self) -> MassKind {
        self.mass_kind.unwrap_or(MassKind::Monoisotopic)
    }
    pub(crate) fn atom_count_mode_or_default(&self) -> AtomCount {
        self.atom_count_mode.unwrap_or(AtomCount::Heavy)
    }
    pub(crate) fn allow_isotopes_or_default(&self) -> bool {
        self.allow_isotopes.unwrap_or(false)
    }
    pub(crate) fn allow_radicals_or_default(&self) -> bool {
        self.allow_radicals.unwrap_or(false)
    }
    pub(crate) fn require_single_component_or_default(&self) -> bool {
        self.require_single_component.unwrap_or(true)
    }
}

/// Builder for [`Filters`]. Every setter is optional; [`build`](FiltersBuilder::build)
/// validates consistency.
///
/// Effective defaults when unset: mass kind = monoisotopic, atom-count mode =
/// heavy, isotopes forbidden, radicals forbidden, single component required, no
/// charge bound, no element set, no size bounds.
#[derive(Debug, Clone, Default)]
pub struct FiltersBuilder {
    inner: Filters,
}

impl FiltersBuilder {
    /// Sets the lower bound (inclusive) on molecular mass in Daltons.
    pub fn min_mass_da(mut self, da: f64) -> Self {
        self.inner.min_mass_da = Some(da);
        self
    }
    /// Sets the upper bound (inclusive) on molecular mass in Daltons.
    pub fn max_mass_da(mut self, da: f64) -> Self {
        self.inner.max_mass_da = Some(da);
        self
    }
    /// Selects which mass definition the bounds apply to (default monoisotopic).
    pub fn mass_kind(mut self, kind: MassKind) -> Self {
        self.inner.mass_kind = Some(kind);
        self
    }
    /// Sets the lower bound (inclusive) on atom count.
    pub fn min_atoms(mut self, n: u32) -> Self {
        self.inner.min_atoms = Some(n);
        self
    }
    /// Sets the upper bound (inclusive) on atom count.
    pub fn max_atoms(mut self, n: u32) -> Self {
        self.inner.max_atoms = Some(n);
        self
    }
    /// Selects whether atom-count bounds count heavy atoms or all atoms
    /// (default heavy).
    pub fn atom_count_mode(mut self, mode: AtomCount) -> Self {
        self.inner.atom_count_mode = Some(mode);
        self
    }
    /// Sets the element whitelist or blacklist.
    pub fn elements(mut self, set: ElementSet) -> Self {
        self.inner.elements = Some(set);
        self
    }
    /// Allows or forbids explicit isotopes (default forbid).
    pub fn allow_isotopes(mut self, yes: bool) -> Self {
        self.inner.allow_isotopes = Some(yes);
        self
    }
    /// Allows or forbids radicals / open-valence atoms (default forbid).
    pub fn allow_radicals(mut self, yes: bool) -> Self {
        self.inner.allow_radicals = Some(yes);
        self
    }
    /// Requires (or not) a single connected component (default require).
    pub fn require_single_component(mut self, yes: bool) -> Self {
        self.inner.require_single_component = Some(yes);
        self
    }
    /// Sets an upper bound on the absolute net formal charge.
    pub fn max_abs_charge(mut self, q: u32) -> Self {
        self.inner.max_abs_charge = Some(q);
        self
    }

    /// Validates and finalizes the [`Filters`].
    ///
    /// # Errors
    ///
    /// Returns [`LosError::Config`] if a min bound exceeds its max, if a mass
    /// bound is non-finite or negative, or if an element set is empty.
    pub fn build(self) -> Result<Filters> {
        let f = self.inner;
        if let (Some(lo), Some(hi)) = (f.min_mass_da, f.max_mass_da)
            && lo > hi
        {
            return Err(LosError::Config(format!(
                "min_mass_da ({lo}) > max_mass_da ({hi})"
            )));
        }
        for (name, v) in [
            ("min_mass_da", f.min_mass_da),
            ("max_mass_da", f.max_mass_da),
        ] {
            if let Some(v) = v
                && (!v.is_finite() || v < 0.0)
            {
                return Err(LosError::Config(format!(
                    "{name} must be finite and non-negative, got {v}"
                )));
            }
        }
        if let (Some(lo), Some(hi)) = (f.min_atoms, f.max_atoms)
            && lo > hi
        {
            return Err(LosError::Config(format!(
                "min_atoms ({lo}) > max_atoms ({hi})"
            )));
        }
        if let Some(set) = &f.elements {
            let empty = match set {
                ElementSet::Whitelist(v) | ElementSet::Blacklist(v) => v.is_empty(),
            };
            if empty {
                return Err(LosError::Config("element set must not be empty".into()));
            }
        }
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply() {
        let f = Filters::builder().build().unwrap();
        assert_eq!(f.mass_kind_or_default(), MassKind::Monoisotopic);
        assert_eq!(f.atom_count_mode_or_default(), AtomCount::Heavy);
        assert!(!f.allow_isotopes_or_default());
        assert!(!f.allow_radicals_or_default());
        assert!(f.require_single_component_or_default());
    }

    #[test]
    fn rejects_inverted_mass_bounds() {
        let e = Filters::builder()
            .min_mass_da(500.0)
            .max_mass_da(100.0)
            .build();
        assert!(matches!(e, Err(LosError::Config(_))));
    }

    #[test]
    fn rejects_inverted_atom_bounds() {
        let e = Filters::builder().min_atoms(40).max_atoms(10).build();
        assert!(matches!(e, Err(LosError::Config(_))));
    }

    #[test]
    fn rejects_empty_element_set() {
        let e = Filters::builder()
            .elements(ElementSet::Whitelist(vec![]))
            .build();
        assert!(matches!(e, Err(LosError::Config(_))));
    }

    #[test]
    fn rejects_negative_mass() {
        let e = Filters::builder().max_mass_da(-1.0).build();
        assert!(matches!(e, Err(LosError::Config(_))));
    }

    #[test]
    fn accepts_reasonable_config() {
        let f = Filters::builder()
            .max_mass_da(900.0)
            .min_atoms(3)
            .max_atoms(70)
            .elements(ElementSet::Whitelist(vec![
                Element::C,
                Element::H,
                Element::O,
            ]))
            .max_abs_charge(0)
            .build()
            .unwrap();
        assert_eq!(f.max_mass_da, Some(900.0));
        assert_eq!(f.max_abs_charge, Some(0));
    }
}
