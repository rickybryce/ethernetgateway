//! Which store an FTP session uses: this gateway's own transfer directory, or
//! -- on a slave -- the master's, through [`super::remote`].  The session
//! calls these and never knows which.

use super::proto::Entry;
use super::remote::{RemoteRead, RemoteStore, RemoteWrite};
use super::store::{LocalStore, StoreError, Upload, WriteMode};

pub enum Store {
    Local(LocalStore),
    Remote(RemoteStore),
}

/// Where a download's bytes come from.
pub enum Source {
    Local(tokio::fs::File),
    Remote(RemoteRead),
}

/// Where an upload's bytes go.
pub enum Sink {
    Local(Upload),
    Remote(RemoteWrite),
}

macro_rules! both {
    ($self:ident, $s:ident => $e:expr) => {
        match $self {
            Store::Local($s) => $e,
            Store::Remote($s) => $e,
        }
    };
}

impl Store {
    pub async fn stat(&self, path: &str) -> Result<Entry, StoreError> {
        both!(self, s => s.stat(path).await)
    }
    pub async fn list(&self, path: &str) -> Result<Vec<Entry>, StoreError> {
        both!(self, s => s.list(path).await)
    }
    pub async fn delete(&self, path: &str) -> Result<(), StoreError> {
        both!(self, s => s.delete(path).await)
    }
    pub async fn mkdir(&self, path: &str) -> Result<(), StoreError> {
        both!(self, s => s.mkdir(path).await)
    }
    pub async fn rmdir(&self, path: &str) -> Result<(), StoreError> {
        both!(self, s => s.rmdir(path).await)
    }
    pub async fn check_rename_source(&self, path: &str) -> Result<(), StoreError> {
        both!(self, s => s.check_rename_source(path).await)
    }
    pub async fn rename(&self, from: &str, to: &str) -> Result<(), StoreError> {
        both!(self, s => s.rename(from, to).await)
    }
    pub async fn unique_name(&self, dir: &str, hint: Option<String>) -> Result<String, StoreError> {
        both!(self, s => s.unique_name(dir, hint).await)
    }
    pub async fn open_read(&self, path: &str, offset: u64) -> Result<Source, StoreError> {
        match self {
            Store::Local(s) => s.open_read(path, offset).await.map(Source::Local),
            Store::Remote(s) => s.open_read(path, offset).await.map(Source::Remote),
        }
    }
    pub async fn begin_write(&self, path: &str, mode: WriteMode) -> Result<Sink, StoreError> {
        match self {
            Store::Local(s) => s.begin_write(path, mode).await.map(Sink::Local),
            Store::Remote(s) => s.begin_write(path, mode).await.map(Sink::Remote),
        }
    }
}

impl Source {
    pub async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use tokio::io::AsyncReadExt;
        match self {
            Source::Local(f) => f.read(buf).await,
            Source::Remote(r) => r.read(buf).await,
        }
    }
}

impl Sink {
    pub async fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        match self {
            Sink::Local(u) => u.file.write_all(data).await,
            Sink::Remote(w) => w.write_all(data).await,
        }
    }
    pub async fn finish(self) -> Result<(), StoreError> {
        match self {
            Sink::Local(u) => u.finish().await,
            Sink::Remote(w) => w.finish().await,
        }
    }
}
