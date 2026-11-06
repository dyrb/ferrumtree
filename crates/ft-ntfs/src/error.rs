use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    NotNtfs,
    BadBootSector(&'static str),
    BadMftRecord(&'static str),
    BadRunlist,
    Unsupported(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::NotNtfs => write!(f, "volume is not NTFS"),
            Error::BadBootSector(w) => write!(f, "invalid NTFS boot sector: {w}"),
            Error::BadMftRecord(w) => write!(f, "invalid $MFT record: {w}"),
            Error::BadRunlist => write!(f, "invalid data run list"),
            Error::Unsupported(w) => write!(f, "unsupported: {w}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
