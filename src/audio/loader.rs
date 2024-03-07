use std::io::Seek;
use std::{collections::HashMap, io::SeekFrom};

use futures_util::{future, stream::futures_unordered::FuturesUnordered, StreamExt};

use librespot_audio::{AudioDecrypt, AudioFile, StreamLoaderController};
use librespot_core::{Session, SpotifyId};
use librespot_metadata::audio::{AudioFileFormat, AudioFiles, AudioItem};
use log::{debug, error, info, warn};

use super::track::{OpenedTrack, Subfile};

// Spotify inserts a custom Ogg packet at the start with custom metadata values, that you would
// otherwise expect in Vorbis comments. This packet isn't well-formed and players may balk at it.
const SPOTIFY_OGG_HEADER_END: u64 = 0xa7;

pub struct TrackLoader {
    session: Session,
    opened_tracks: HashMap<SpotifyId, OpenedTrack>,
}

impl TrackLoader {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            opened_tracks: HashMap::new(),
        }
    }
    pub fn get_opened(&self, track: &SpotifyId) -> Option<&OpenedTrack> {
        self.opened_tracks.get(track)
    }
    pub fn get_opened_mut(&mut self, track: &SpotifyId) -> Option<&mut OpenedTrack> {
        self.opened_tracks.get_mut(track)
    }
    pub fn close(&mut self, track: &SpotifyId) -> Result<(), String> {
        if let Some(loaded_track) = self.get_opened(track) {
            if loaded_track.decr_ref() <= 1 {
                self.opened_tracks
                    .remove(track)
                    .ok_or("Cannot close opened track".to_owned())?;
            }
            Ok(())
        } else {
            Err("No track is currently open".to_string())
        }
    }
    pub fn seek(&mut self, track: &SpotifyId, position: u64) -> Result<u64, String> {
        if let Some(loaded_track) = self.get_opened_mut(track) {
            loaded_track
                .seek(SeekFrom::Start(position))
                .map_err(|e| e.to_string())
        } else {
            Err("No track is currently open".to_owned())
        }
    }
    async fn find_available_alternative(&self, audio_item: AudioItem) -> Option<AudioItem> {
        if let Err(e) = audio_item.availability {
            error!("Track is unavailable: {}", e);
            None
        } else if !audio_item.files.is_empty() {
            Some(audio_item)
        } else if let Some(alternatives) = &audio_item.alternatives {
            let alternatives: FuturesUnordered<_> = alternatives
                .iter()
                .map(|alt_id| AudioItem::get_file(&self.session, *alt_id))
                .collect();

            alternatives
                .filter_map(|x| future::ready(x.ok()))
                .filter(|x| future::ready(x.availability.is_ok()))
                .next()
                .await
        } else {
            error!("Track should be available, but no alternatives found.");
            None
        }
    }

    fn stream_data_rate(format: AudioFileFormat) -> usize {
        let kbps = match format {
            AudioFileFormat::OGG_VORBIS_96 => 12,
            AudioFileFormat::OGG_VORBIS_160 => 20,
            AudioFileFormat::OGG_VORBIS_320 => 40,
            AudioFileFormat::MP3_256 => 32,
            AudioFileFormat::MP3_320 => 40,
            AudioFileFormat::MP3_160 => 20,
            AudioFileFormat::MP3_96 => 12,
            AudioFileFormat::MP3_160_ENC => 20,
            AudioFileFormat::AAC_24 => 3,
            AudioFileFormat::AAC_48 => 6,
            AudioFileFormat::FLAC_FLAC => 112, // assume 900 kbit/s on average
        };
        kbps * 1024
    }

    async fn load_track(
        &self,
        spotify_id: SpotifyId,
    ) -> Option<(
        Subfile<AudioDecrypt<AudioFile>>,
        AudioFileFormat,
        StreamLoaderController,
    )> {
        let audio_item = match AudioItem::get_file(&self.session, spotify_id).await {
            Ok(audio) => match self.find_available_alternative(audio).await {
                Some(audio) => audio,
                None => {
                    warn!(
                        "<{}> is not available",
                        spotify_id.to_uri().unwrap_or_default()
                    );
                    return None;
                }
            },
            Err(e) => {
                error!("Unable to load audio item: {:?}", e);
                return None;
            }
        };

        info!(
            "Loading <{}> with Spotify URI <{}>",
            audio_item.name, audio_item.uri
        );

        // (Most) podcasts seem to support only 96 kbps Ogg Vorbis, so fall back to it
        let formats = [
            AudioFileFormat::MP3_320,
            AudioFileFormat::OGG_VORBIS_320,
            AudioFileFormat::MP3_256,
            AudioFileFormat::MP3_160,
            AudioFileFormat::OGG_VORBIS_160,
            AudioFileFormat::MP3_96,
            AudioFileFormat::OGG_VORBIS_96,
        ];

        debug!("Available audio file: {:?}", audio_item.files);

        let (format, file_id) =
            match formats
                .iter()
                .find_map(|format| match audio_item.files.get(format) {
                    Some(&file_id) => Some((*format, file_id)),
                    _ => None,
                }) {
                Some(t) => t,
                None => {
                    warn!(
                        "<{}> is not available in any supported format",
                        audio_item.name
                    );
                    return None;
                }
            };

        let bytes_per_second = Self::stream_data_rate(format);
        info!(
            "Byte per second: {:?}, file ID: {:}",
            bytes_per_second, file_id
        );

        // // This is only a loop to be able to reload the file if an error occurred
        // // while opening a cached file.
        // loop {
        let encrypted_file = AudioFile::open(&self.session, file_id, 10240);

        let encrypted_file = match encrypted_file.await {
            Ok(encrypted_file) => encrypted_file,
            Err(e) => {
                error!("Unable to load encrypted file: {:?}", e);
                return None;
            }
        };

        let stream_loader_controller = encrypted_file.get_stream_loader_controller().ok()?;

        // Not all audio files are encrypted. If we can't get a key, try loading the track
        // without decryption. If the file was encrypted after all, the decoder will fail
        // parsing and bail out, so we should be safe from outputting ear-piercing noise.
        let key = match self.session.audio_key().request(spotify_id, file_id).await {
            Ok(key) => Some(key),
            Err(e) => {
                warn!("Unable to load key, continuing without decryption: {}", e);
                None
            }
        };
        let decrypted_file = AudioDecrypt::new(key, encrypted_file);

        let is_ogg_vorbis = AudioFiles::is_ogg_vorbis(format);
        let offset = if is_ogg_vorbis {
            // Spotify stores normalisation data in a custom
            SPOTIFY_OGG_HEADER_END
        } else {
            0
        };
        let audio_file = match Subfile::new(
            decrypted_file,
            offset,
            stream_loader_controller.len() as u64,
        ) {
            Ok(audio_file) => audio_file,
            Err(e) => {
                error!("PlayerTrackLoader::load_track error opening subfile: {}", e);
                return None;
            }
        };

        info!(
            "<{}> ({} bytes) loaded",
            audio_item.name,
            stream_loader_controller.len()
        );

        stream_loader_controller.set_random_access_mode();
        // stream_loader_controller.set_stream_mode();

        // TODO use a buffer instead of full read
        stream_loader_controller.range_to_end_available();
        // stream_loader_controller.fetch(Range { start: 0, length: stream_loader_controller.len() });

        Some((audio_file, format, stream_loader_controller))
        // }
    }

    pub async fn open(&mut self, track: SpotifyId) -> Result<(i64, AudioFileFormat), String> {
        if let Some(loaded_track) = self.opened_tracks.get(&track) {
            loaded_track.incr_ref();
            return Ok((loaded_track.len() as i64, loaded_track.format()));
        }

        if let Some((file, format, controller)) = self.load_track(track).await {
            let filesize = controller.len();
            self.opened_tracks
                .insert(track, OpenedTrack::new(Box::new(file), controller, format));
            Ok((filesize as i64, format))
        } else {
            Err("unable to load track".to_owned())
        }
    }
}
