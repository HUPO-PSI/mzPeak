use mzdata::params::ParamDescribed;




/// A type alias over a tuple of (primary source id, <entity>)
#[derive(Debug, Clone, Default)]
pub struct Indexed<T>(pub u64, pub T);
/// A type alias over a tuple of (primary source id, optional secondary id, <entity>)
#[derive(Debug, Clone, Default)]
pub struct DoubleIndexed<T>(pub u64, pub Option<u64>, pub T);

/// A helper trait for handling (multi-)-relationship keyed visitors over type `T`
pub trait CompoundIndexVisitor<T> {
    /// The primary source entity's id key
    fn source_index_mut(&mut self) -> &mut u64;

    /// The (potentially absent) secondary id key
    fn secondary_index_mut(&mut self) -> Option<&mut u64>;

    /// Get a mutable access to the actual thing we are visiting to build
    fn description_mut(&mut self) -> &mut T;

    /// Take the entity out of the container
    fn into_description(self) -> T;

    /// The primary source entity's id key
    fn source_index(&self) -> u64;

    /// The (potentially absent) secondary id key
    fn secondary_index(&self) -> Option<u64>;

    /// Unpack the container into a tuple of id keys and the entity description
    fn unpack(self) -> (u64, Option<u64>, T)
    where
        Self: Sized,
    {
        (
            self.source_index(),
            self.secondary_index(),
            self.into_description(),
        )
    }
}

impl<T> CompoundIndexVisitor<T> for Indexed<T> {
    fn description_mut(&mut self) -> &mut T {
        &mut self.1
    }

    fn source_index_mut(&mut self) -> &mut u64 {
        &mut self.0
    }

    fn secondary_index_mut(&mut self) -> Option<&mut u64> {
        None
    }

    fn into_description(self) -> T {
        self.1
    }

    fn source_index(&self) -> u64 {
        self.0
    }

    fn secondary_index(&self) -> Option<u64> {
        None
    }
}

impl<T> CompoundIndexVisitor<T> for DoubleIndexed<T> {
    fn description_mut(&mut self) -> &mut T {
        &mut self.2
    }

    fn source_index_mut(&mut self) -> &mut u64 {
        &mut self.0
    }

    fn secondary_index_mut(&mut self) -> Option<&mut u64> {
        self.1.as_mut()
    }

    fn into_description(self) -> T {
        self.2
    }

    fn source_index(&self) -> u64 {
        self.0
    }

    fn secondary_index(&self) -> Option<u64> {
        self.1
    }
}

impl<T: ParamDescribed> ParamDescribed for Indexed<T> {
    fn params(&self) -> &[mzdata::Param] {
        self.1.params()
    }

    fn params_mut(&mut self) -> &mut mzdata::ParamList {
        self.1.params_mut()
    }
}

impl<T: ParamDescribed> ParamDescribed for DoubleIndexed<T> {
    fn params(&self) -> &[mzdata::Param] {
        self.2.params()
    }

    fn params_mut(&mut self) -> &mut mzdata::ParamList {
        self.2.params_mut()
    }
}


/// Enclose the parallel arrays of "descriptions" and their offsets so that the borrow
/// checker knows that method calls on this instance aren't tied to the owning objects
pub struct OffsetCollection<'a, T> {
    pub(crate) descriptions: &'a mut [T],
    pub(crate) offsets: &'a [usize],
}

impl<'a, T> IntoIterator for OffsetCollection<'a, T> {
    type Item = (usize, &'a mut T);

    type IntoIter =
        std::iter::Zip<std::iter::Copied<std::slice::Iter<'a, usize>>, std::slice::IterMut<'a, T>>;

    #[inline(always)]
    fn into_iter(self) -> Self::IntoIter {
        assert!(self.offsets.len() <= self.descriptions.len());
        self.offsets.iter().copied().zip(&mut *self.descriptions)
    }
}

impl<'a, T> OffsetCollection<'a, T> {
    pub fn new(descriptions: &'a mut [T], offsets: &'a [usize]) -> Self {
        Self {
            descriptions,
            offsets,
        }
    }
}

pub struct OffsetCollectionSubGroup<'a, T, U: 'a>(OffsetCollection<'a, T>, Box<dyn FnMut((usize, &'a mut T)) -> (usize, &'a mut U)>);

impl<'a, T, U: 'a> IntoIterator for OffsetCollectionSubGroup<'a, T, U> {
    type Item = (usize, &'a mut U);

    type IntoIter = std::iter::Map<std::iter::Zip<std::iter::Copied<std::slice::Iter<'a, usize>>, std::slice::IterMut<'a, T>>, Box<dyn FnMut((usize, &'a mut T)) -> (usize, &'a mut U)>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter().map(self.1)
    }
}

impl<'a, T, U: 'a> OffsetCollectionSubGroup<'a, T, U> {
    pub fn new(offset_collection: OffsetCollection<'a, T>, func: Box<dyn FnMut((usize, &'a mut T)) -> (usize, &'a mut U)>) -> Self {
        Self(offset_collection, func)
    }
}

