use pyo3::prelude::*;
use pyo3::types::PySlice as PySliceType;

use sqlfluffrs_types::slice::Slice as RsSlice;

#[derive(Clone, Copy, Debug)]
pub struct PySlice(pub RsSlice);

impl From<PySlice> for RsSlice {
    fn from(value: PySlice) -> Self {
        value.0
    }
}

impl From<RsSlice> for PySlice {
    fn from(value: RsSlice) -> Self {
        PySlice(value)
    }
}

impl<'a, 'py> FromPyObject<'a, 'py> for PySlice {
    type Error = PyErr;

    fn extract(obj: pyo3::Borrowed<'a, 'py, pyo3::PyAny>) -> Result<Self, Self::Error> {
        let start = obj.getattr("start")?.extract::<usize>()?;
        let stop = obj.getattr("stop")?.extract::<usize>()?;
        Ok(PySlice(RsSlice { start, stop }))
    }
}

impl<'py> IntoPyObject<'py> for PySlice {
    type Target = PySliceType;
    type Output = Bound<'py, Self::Target>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        // Build ``slice(start, stop)`` with step=None. Native code constructs
        // 2-arg slices everywhere, and ``slice(a, b, 1) != slice(a, b, None)``
        // in Python — a step-1 slice here silently broke every downstream
        // equality against a native-built slice (observed: FixPatch dedupe
        // over repeated jinja-loop regions applied a patch twice).
        let ty = py.get_type::<PySliceType>();
        let obj = ty.call1((self.0.start, self.0.stop))?;
        obj.cast_into::<PySliceType>().map_err(PyErr::from)
    }
}

impl std::fmt::Display for PySlice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "slice({}, {}, None)", self.0.start, self.0.stop)
    }
}
