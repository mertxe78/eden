/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use crate::dagalgo::dagalgo;
use crate::idmap;
use crate::Names;
use crate::Spans;
use anyhow::Result;
use cpython::*;
use cpython_async::futures;
use cpython_async::TStream;
use cpython_ext::convert::BytesLike;
use cpython_ext::ExtractInner;
use cpython_ext::PyNone;
use cpython_ext::PyPath;
use cpython_ext::ResultPyErrExt;
use cpython_ext::Str;
use dag::delegate;
use dag::ops::IdConvert;
use dag::ops::PrefixLookup;
use dag::ops::ToIdSet;
use dag::ops::ToSet;
use dag::DagAlgorithm;
use dag::Vertex;
use futures::stream::TryStreamExt;
use hgcommits::AppendCommits;
use hgcommits::DescribeBackend;
use hgcommits::DoubleWriteCommits;
use hgcommits::GitSegmentedCommits;
use hgcommits::HgCommit;
use hgcommits::HgCommits;
use hgcommits::HybridCommits;
use hgcommits::MemHgCommits;
use hgcommits::ReadCommitText;
use hgcommits::RevlogCommits;
use hgcommits::StreamCommitText;
use hgcommits::StripCommits;
use minibytes::Bytes;
use pyedenapi::PyClient;
use pymetalog::metalog as PyMetaLog;
use std::cell::RefCell;

/// A combination of other traits: commit read/write + DAG algorithms.
pub trait Commits:
    ReadCommitText
    + StripCommits
    + AppendCommits
    + DescribeBackend
    + DagAlgorithm
    + IdConvert
    + StreamCommitText
    + PrefixLookup
    + ToIdSet
    + ToSet
{
}

impl Commits for HgCommits {}
impl Commits for HybridCommits {}
impl Commits for MemHgCommits {}
impl Commits for RevlogCommits {}
impl Commits for DoubleWriteCommits {}
impl Commits for GitSegmentedCommits {}

py_class!(pub class commits |py| {
    data inner: RefCell<Box<dyn Commits + Send + 'static>>;

    /// Add a list of commits (node, [parent], text) in-memory.
    def addcommits(&self, commits: Vec<(PyBytes, Vec<PyBytes>, PyBytes)>) -> PyResult<PyNone> {
        let commits: Vec<HgCommit> = commits.into_iter().map(|(node, parents, raw_text)| {
            let vertex = node.data(py).to_vec().into();
            let parents = parents.into_iter().map(|p| p.data(py).to_vec().into()).collect();
            let raw_text = raw_text.data(py).to_vec().into();
            HgCommit { vertex, parents, raw_text }
        }).collect();
        let mut inner = self.inner(py).borrow_mut();
        inner.add_commits(&commits).map_pyerr(py)?;
        Ok(PyNone)
    }

    /// Flush in-memory commit data and graph to disk.
    /// `masterheads` is a hint about what parts belong to the "master" group.
    def flush(&self, masterheads: Vec<PyBytes>) -> PyResult<PyNone> {
        let heads = masterheads.into_iter().map(|h| h.data(py).to_vec().into()).collect::<Vec<_>>();
        let mut inner = self.inner(py).borrow_mut();
        inner.flush(&heads).map_pyerr(py)?;
        Ok(PyNone)
    }

    /// Flush in-memory commit data to disk.
    /// For the revlog backend, this also write the commit graph to disk.
    def flushcommitdata(&self) -> PyResult<PyNone> {
        let mut inner = self.inner(py).borrow_mut();
        inner.flush_commit_data().map_pyerr(py)?;
        Ok(PyNone)
    }

    /// Strip commits. ONLY used to make LEGACY TESTS running.
    /// Fails if called in a non-test environment.
    /// New tests should avoid depending on `strip`.
    def strip(&self, set: Names) -> PyResult<PyNone> {
        let mut inner = self.inner(py).borrow_mut();
        inner.strip_commits(set.0).map_pyerr(py)?;
        Ok(PyNone)
    }

    /// Lookup the raw text of a commit by binary commit hash.
    def getcommitrawtext(&self, node: PyBytes) -> PyResult<Option<PyBytes>> {
        let vertex = node.data(py).to_vec().into();
        let inner = self.inner(py).borrow();
        let optional_bytes = inner.get_commit_raw_text(&vertex).map_pyerr(py)?;
        Ok(optional_bytes.map(|bytes| PyBytes::new(py, bytes.as_ref())))
    }

    /// Lookup the raw texts of a stream of binary commit hashes.
    /// Return a stream of (node, rawtext).
    def streamcommitrawtext(&self, stream: TStream<anyhow::Result<BytesLike<Vertex>>>)
        -> PyResult<TStream<anyhow::Result<(BytesLike<Vertex>, BytesLike<Bytes>)>>>
    {
        let inner = self.inner(py).borrow();
        let stream = Box::pin(stream.stream().map_ok(|bytes_like| bytes_like.0));
        let output_stream = inner.stream_commit_raw_text(stream).map_pyerr(py)?;
        Ok(output_stream.map_ok(|c| (BytesLike(c.vertex), BytesLike(c.raw_text))).into())
    }

    /// Convert Set to IdSet. For compatibility with legacy code only.
    def torevs(&self, set: Names) -> PyResult<Spans> {
        let inner = self.inner(py).borrow();
        Ok(Spans(inner.to_id_set(&set.0).map_pyerr(py)?))
    }

    /// Convert IdSet to Set. For compatibility with legacy code only.
    def tonodes(&self, set: Spans) -> PyResult<Names> {
        let inner = self.inner(py).borrow();
        Ok(Names(inner.to_set(&set.0).map_pyerr(py)?))
    }

    /// Obtain the read-only dagalgo object that supports various DAG algorithms.
    def dagalgo(&self) -> PyResult<dagalgo> {
        dagalgo::from_dag(py, self.clone_ref(py))
    }

    /// Obtain the read-only object that can do hex prefix lookup and convert
    /// between binary commit hashes and integer Ids.
    def idmap(&self) -> PyResult<idmap::idmap> {
        idmap::idmap::from_idmap(py, self.clone_ref(py))
    }

    /// Name of the backend used for DAG algorithms.
    def algorithmbackend(&self) -> PyResult<Str> {
        let inner = self.inner(py).borrow();
        Ok(inner.algorithm_backend().to_string().into())
    }

    /// Describe the backend.
    def describebackend(&self) -> PyResult<Str> {
        let inner = self.inner(py).borrow();
        Ok(inner.describe_backend().into())
    }

    /// Explain internal data.
    def explaininternals(&self, out: PyObject) -> PyResult<PyNone> {
        // This function takes a 'out' parameter so it can work with pager
        // and output progressively.
        let inner = self.inner(py).borrow();
        let mut out = cpython_ext::wrap_pyio(out);
        inner.explain_internals(&mut out).map_pyerr(py)?;
        Ok(PyNone)
    }

    /// Construct `commits` from a revlog (`00changelog.i` and `00changelog.d`).
    @staticmethod
    def openrevlog(dir: &PyPath) -> PyResult<Self> {
        let inner = RevlogCommits::new(dir.as_path()).map_pyerr(py)?;
        Self::from_commits(py, inner)
    }

    /// Construct `commits` from a segmented changelog + hgcommits directory.
    @staticmethod
    def opensegments(segmentsdir: &PyPath, commitsdir: &PyPath) -> PyResult<Self> {
        let inner = HgCommits::new(segmentsdir.as_path(), commitsdir.as_path()).map_pyerr(py)?;
        Self::from_commits(py, inner)
    }

    /// Migrate from revlog to segmented changelog (full IdMap).
    ///
    /// This does not migrate commit texts and therefore only useful for
    /// doublewrite backend.
    @staticmethod
    def migraterevlogtosegments(revlogdir: &PyPath, segmentsdir: &PyPath, commitsdir: &PyPath, master: Names) -> PyResult<PyNone> {
        let revlog = RevlogCommits::new(revlogdir.as_path()).map_pyerr(py)?;
        let mut segments = HgCommits::new(segmentsdir.as_path(), commitsdir.as_path()).map_pyerr(py)?;
        py.allow_threads(|| segments.import_dag(revlog, master.0)).map_pyerr(py)?;
        Ok(PyNone)
    }

    /// Construct "double write" `commits` from both revlog and segmented
    /// changelog.
    @staticmethod
    def opendoublewrite(revlogdir: &PyPath, segmentsdir: &PyPath, commitsdir: &PyPath) -> PyResult<Self> {
        let inner = DoubleWriteCommits::new(revlogdir.as_path(), segmentsdir.as_path(), commitsdir.as_path()).map_pyerr(py)?;
        Self::from_commits(py, inner)
    }

    /// Construct `commits` from a revlog + segmented changelog + hgcommits + edenapi hybrid.
    ///
    /// This is similar to doublewrite backend, except that commit text fallback is edenapi,
    /// not revlog, despite the revlog might have the data.
    @staticmethod
    def openhybrid(
        revlogdir: &PyPath, segmentsdir: &PyPath, commitsdir: &PyPath, edenapi: PyClient, reponame: String
    ) -> PyResult<Self> {
        let client = edenapi.extract_inner(py);
        let inner = HybridCommits::new(
            revlogdir.as_path(),
            segmentsdir.as_path(),
            commitsdir.as_path(),
            client,
            reponame,
        ).map_pyerr(py)?;
        Self::from_commits(py, inner)
    }

    /// Construct "git segmented" `commits` from a git repo and segmented
    /// changelog.
    @staticmethod
    def opengitsegments(gitdir: &PyPath, segmentsdir: &PyPath, metalog: PyMetaLog) -> PyResult<Self> {
        let inner = py.allow_threads(|| GitSegmentedCommits::new(gitdir.as_path(), segmentsdir.as_path())).map_pyerr(py)?;
        let meta = metalog.metalog_refcell(py);
        let mut meta = meta.borrow_mut();
        inner.export_git_references(&mut meta).map_pyerr(py)?;
        Self::from_commits(py, inner)
    }

    /// Construct a private, empty `commits` object backed by the memory.
    /// `flush` does nothing for this type of object.
    @staticmethod
    def openmemory() -> PyResult<Self> {
        let inner = MemHgCommits::new().map_pyerr(py)?;
        Self::from_commits(py, inner)
    }
});

impl commits {
    /// Create a `commits` Python object from a Rust struct.
    pub fn from_commits(py: Python, commits: impl Commits + Send + 'static) -> PyResult<Self> {
        Self::create_instance(py, RefCell::new(Box::new(commits)))
    }
}

// Delegate trait implementations to `inner`.
// commits are used by other Python objects: the other Python objects hold the GIL.
delegate!(PrefixLookup | IdConvert | DagAlgorithm, commits => self.inner(unsafe { Python::assume_gil_acquired() }).borrow());
