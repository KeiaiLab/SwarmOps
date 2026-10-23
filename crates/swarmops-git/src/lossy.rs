//! The classes of git content a changeset cannot hold as git has it (storage spec §7), and sets of
//! them: the classes an import allows, and those a commit lost.

use std::fmt;

/// A class of git content that a changeset cannot hold as git has it (storage spec §7): a commit
/// holding any no longer renders back to its own id. The conversion refuses such a commit unless
/// its class is allowed, and otherwise drops or normalizes the content and names the class in the
/// changeset's `git.lossy` metadata ([`convert`](crate::convert) says how, class by class).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LossyClass {
    /// A submodule's entry, a gitlink (mode `160000`), which names a commit of another
    /// repository: dropped.
    Gitlinks,
    /// A name that is no path component: not UTF-8, `.` or `..`, empty, or holding a `/`:
    /// dropped.
    Paths,
    /// A mode of a file, a directory or a symlink written otherwise than git writes it (`100644`,
    /// `100755`, `40000`, `120000`), such as `100664` or a zero-padded `040000`: normalized, a
    /// regular file's to a file or an executable by its owner's execute bit.
    Modes,
    /// A tree whose entries are not in git's order, or that holds a name twice: put in order, the
    /// first entry of a name kept.
    TreeOrder,
    /// An entry naming an empty tree: dropped.
    EmptyTrees,
    /// A commit whose bytes its fields cannot give back: an ident, a time or a header line git
    /// writes otherwise, no blank line before the message, or a parent listed twice (kept once).
    Nonstandard,
}

impl LossyClass {
    /// Every class, in declaration order.
    pub const ALL: [LossyClass; 6] = [
        LossyClass::Gitlinks,
        LossyClass::Paths,
        LossyClass::Modes,
        LossyClass::TreeOrder,
        LossyClass::EmptyTrees,
        LossyClass::Nonstandard,
    ];

    /// The class's name, as `--lossy` and the `git.lossy` metadata spell it: `gitlinks`, `paths`,
    /// `modes`, `tree_order`, `empty_trees` or `nonstandard`.
    pub fn as_str(self) -> &'static str {
        match self {
            LossyClass::Gitlinks => "gitlinks",
            LossyClass::Paths => "paths",
            LossyClass::Modes => "modes",
            LossyClass::TreeOrder => "tree_order",
            LossyClass::EmptyTrees => "empty_trees",
            LossyClass::Nonstandard => "nonstandard",
        }
    }

    /// The class's bit in a [`LossySet`]: its place in [`LossyClass::ALL`].
    fn bit(self) -> u8 {
        1 << self as u8
    }
}

impl fmt::Display for LossyClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The classes in the order of their names: the order a [`LossySet`] iterates in and writes its
/// metadata in.
const BY_NAME: [LossyClass; 6] = [
    LossyClass::EmptyTrees,
    LossyClass::Gitlinks,
    LossyClass::Modes,
    LossyClass::Nonstandard,
    LossyClass::Paths,
    LossyClass::TreeOrder,
];

/// A set of [`LossyClass`]es: the classes a conversion allows, or the ones a commit lost.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct LossySet(u8);

impl LossySet {
    /// No class.
    pub fn empty() -> LossySet {
        LossySet(0)
    }

    /// Every class: `--lossy all`.
    pub fn all() -> LossySet {
        LossyClass::ALL.into_iter().collect()
    }

    /// Whether `class` is in the set.
    pub fn contains(self, class: LossyClass) -> bool {
        self.0 & class.bit() != 0
    }

    /// Puts `class` in the set; whether it was not in it before.
    pub fn insert(&mut self, class: LossyClass) -> bool {
        let added = !self.contains(class);
        self.0 |= class.bit();
        added
    }

    /// The classes of either set.
    #[must_use]
    pub fn union(self, other: LossySet) -> LossySet {
        LossySet(self.0 | other.0)
    }

    /// Whether the set holds no class.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The classes in the set, in the order of their names ([`LossyClass::as_str`]).
    pub fn iter(self) -> impl Iterator<Item = LossyClass> {
        BY_NAME
            .into_iter()
            .filter(move |&class| self.contains(class))
    }

    /// The value of the `git.lossy` metadata of a changeset that lost these classes: their names
    /// in order ([`iter`](LossySet::iter)), joined by `,`, such as `empty_trees,gitlinks`.
    pub fn to_metadata(self) -> Vec<u8> {
        let names: Vec<&str> = self.iter().map(LossyClass::as_str).collect();
        names.join(",").into_bytes()
    }
}

impl FromIterator<LossyClass> for LossySet {
    fn from_iter<I: IntoIterator<Item = LossyClass>>(classes: I) -> LossySet {
        let mut set = LossySet::empty();
        for class in classes {
            set.insert(class);
        }
        set
    }
}

/// The classes in the order of their names, as a set prints them: `{Gitlinks, Modes}`.
impl fmt::Debug for LossySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_name_is_every_class_in_the_order_of_its_name() {
        let mut sorted = LossyClass::ALL;
        sorted.sort_by_key(|class| class.as_str());
        assert_eq!(sorted, BY_NAME);
    }

    #[test]
    fn a_set_holds_what_it_is_given_and_names_it_in_order() {
        let mut set = LossySet::empty();
        assert!(set.is_empty() && set.to_metadata().is_empty());
        assert!(set.insert(LossyClass::TreeOrder));
        assert!(set.insert(LossyClass::Gitlinks));
        assert!(!set.insert(LossyClass::Gitlinks), "in the set already");
        assert!(set.contains(LossyClass::TreeOrder) && !set.contains(LossyClass::Paths));
        assert_eq!(set.to_metadata(), b"gitlinks,tree_order");
        assert_eq!(format!("{set:?}"), "{Gitlinks, TreeOrder}");
        let other: LossySet = [LossyClass::EmptyTrees].into_iter().collect();
        let union = set.union(other);
        assert_eq!(union.to_metadata(), b"empty_trees,gitlinks,tree_order");
        let names: Vec<&str> = LossySet::all().iter().map(LossyClass::as_str).collect();
        assert_eq!(
            names,
            [
                "empty_trees",
                "gitlinks",
                "modes",
                "nonstandard",
                "paths",
                "tree_order"
            ]
        );
    }
}
