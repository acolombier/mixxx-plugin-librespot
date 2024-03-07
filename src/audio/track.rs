use std::{
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicU16, Ordering},
};

use librespot_audio::StreamLoaderController;
use librespot_metadata::audio::AudioFileFormat;

pub trait SeekRead: Seek + Read {}
impl<T: Seek + Read> SeekRead for T {}
pub struct OpenedTrack {
    file: Box<dyn SeekRead + Send + Sync>,
    controller: StreamLoaderController,
    ref_count: AtomicU16,
    audio_format: AudioFileFormat,
}

impl OpenedTrack {
    pub fn new(
        file: Box<dyn SeekRead + Send + Sync>,
        controller: StreamLoaderController,
        audio_format: AudioFileFormat,
    ) -> Self {
        Self {
            file,
            controller,
            audio_format,
            ref_count: AtomicU16::new(1),
        }
    }
    pub fn incr_ref(&self) -> u16 {
        self.ref_count.fetch_add(1, Ordering::AcqRel)
    }
    pub fn decr_ref(&self) -> u16 {
        self.ref_count.fetch_sub(1, Ordering::AcqRel)
    }
    pub fn len(&self) -> usize {
        self.controller.len()
    }
    pub fn format(&self) -> AudioFileFormat {
        self.audio_format
    }
}

pub struct Subfile<T: Read + Seek> {
    stream: T,
    offset: u64,
    length: u64,
}

impl<T: Read + Seek> Subfile<T> {
    pub fn new(mut stream: T, offset: u64, length: u64) -> Result<Subfile<T>, std::io::Error> {
        let target = SeekFrom::Start(offset);
        stream.seek(target)?;

        Ok(Subfile {
            stream,
            offset,
            length,
        })
    }
}

impl<T: Read + Seek> Read for Subfile<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stream.read(buf)
    }
}

impl<T: Read + Seek> Seek for Subfile<T> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let pos = match pos {
            SeekFrom::Start(offset) => SeekFrom::Start(offset + self.offset),
            SeekFrom::End(offset) => {
                if (self.length as i64 - offset) < self.offset as i64 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "newpos would be < self.offset",
                    ));
                }
                pos
            }
            _ => pos,
        };

        let newpos = self.stream.seek(pos)?;
        Ok(newpos - self.offset)
    }
}

impl Read for OpenedTrack {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for OpenedTrack {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(pos)
    }
}
