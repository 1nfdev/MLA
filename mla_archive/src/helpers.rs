/// Helpers for common operation with MLA Archives
use super::{ArchiveFileBlock, ArchiveFileID, ArchiveReader, ArchiveWriter, Error};
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// Extract an Archive linearly.
///
/// `export` maps filenames to Write objects, which will receives the
/// corresponding file's content. If a file is in the archive but not in
/// `export`, this file will be silently ignored.
///
/// This is an effective way to extract all elements from an MLA Archive. It
/// avoids seeking for each files, and for each files parts if files are
/// interleaved. For an MLA Archive, seeking could be a costly operation, and might
/// involve reading data to `Sink` (seeking in decompression), or involves
/// additional computation (getting a whole encrypted block to check its
/// encryption tag).
/// Linear extraction avoids these costs by reading once and only once each byte,
/// and by reducing the amount of seeks.
pub fn linear_extract<W1: Write, R: Read + Seek, S: BuildHasher>(
    archive: &mut ArchiveReader<R>,
    export: &mut HashMap<&String, W1, S>,
) -> Result<(), Error> {
    // Seek at the beginning
    archive.src.seek(SeekFrom::Start(0))?;

    // Use a BufReader to cache, by merging them into one bigger read, small
    // read calls (like the ones on ArchiveFileBlock reading)
    let mut src = io::BufReader::new(&mut archive.src);

    // Associate an ID in the archive to the corresponding filename
    // Do not directly associate to the writer to keep an easier fn API
    let mut id2filename: HashMap<ArchiveFileID, String> = HashMap::new();

    'read_block: loop {
        match ArchiveFileBlock::from(&mut src)? {
            ArchiveFileBlock::FileStart { filename, id } => {
                // If the starting file is meant to be extracted, get the
                // corresponding writer
                if export.contains_key(&filename) {
                    id2filename.insert(id, filename.clone());
                }
            }
            ArchiveFileBlock::EndOfFile { id, .. } => {
                // Drop the corresponding writer
                id2filename.remove(&id);
            }
            ArchiveFileBlock::FileContent { length, id, .. } => {
                // Write a block to the corresponding output, if any

                let copy_src = &mut (&mut src).take(length);
                // Is the file considered?
                let mut extracted: bool = false;
                if let Some(fname) = id2filename.get(&id) {
                    if let Some(writer) = export.get_mut(fname) {
                        io::copy(copy_src, writer)?;
                        extracted = true;
                    }
                };
                if !extracted {
                    // Exhaust the block to Sink to forward the reader
                    io::copy(copy_src, &mut io::sink())?;
                }
            }
            ArchiveFileBlock::EndOfArchiveData {} => {
                // Proper termination
                break 'read_block;
            }
        }
    }
    Ok(())
}

/// Provides a Write interface on an ArchiveWriter file
///
/// This interface is meant to be used in situations where length of the data
/// source is unknown, such as a stream. One can then use the `io::copy`
/// facilities to perform multiples block addition in the archive
pub struct StreamWriter<'a, 'b, W: Write> {
    archive: &'b mut ArchiveWriter<'a, W>,
    file_id: ArchiveFileID,
}

impl<'a, 'b, W: Write> StreamWriter<'a, 'b, W> {
    pub fn new(archive: &'b mut ArchiveWriter<'a, W>, file_id: ArchiveFileID) -> Self {
        Self { archive, file_id }
    }
}

impl<'a, 'b, W: Write> Write for StreamWriter<'a, 'b, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.archive
            .append_file_content(self.file_id, buf.len() as u64, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.archive.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::build_archive;
    use crate::*;
    use std::io::Cursor;

    #[test]
    fn full_linear_extract() {
        // Build an archive with 3 files
        let (mla, key, files) = build_archive(None, false);

        // Prepare the reader
        let dest = Cursor::new(mla.into_raw());
        let mut config = ArchiveReaderConfig::new();
        config.add_private_keys(std::slice::from_ref(&key));
        let mut mla_read = ArchiveReader::from_config(dest, config).unwrap();

        // Prepare writers
        let file_list: Vec<String> = mla_read
            .list_files()
            .expect("reader.list_files")
            .cloned()
            .collect();
        let mut export: HashMap<&String, Vec<u8>> =
            file_list.iter().map(|fname| (fname, Vec::new())).collect();
        linear_extract(&mut mla_read, &mut export).expect("Extract error");

        // Check file per file
        for (fname, content) in files.iter() {
            assert_eq!(export.get(fname).unwrap(), content);
        }
    }

    #[test]
    fn one_linear_extract() {
        // Build an archive with 3 files
        let (mla, key, files) = build_archive(None, false);

        // Prepare the reader
        let dest = Cursor::new(mla.into_raw());
        let mut config = ArchiveReaderConfig::new();
        config.add_private_keys(std::slice::from_ref(&key));
        let mut mla_read = ArchiveReader::from_config(dest, config).unwrap();

        // Prepare writers
        let mut export: HashMap<&String, Vec<u8>> = HashMap::new();
        export.insert(&files[0].0, Vec::new());
        linear_extract(&mut mla_read, &mut export).expect("Extract error");

        // Check file
        assert_eq!(export.get(&files[0].0).unwrap(), &files[0].1);
    }

    #[test]
    fn stream_writer() {
        let file = Vec::new();
        let mut mla = ArchiveWriter::from_config(file, ArchiveWriterConfig::new())
            .expect("Writer init failed");

        let fake_file = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

        // Using write API
        let id = mla.start_file("my_file").unwrap();
        let mut sw = StreamWriter::new(&mut mla, id);
        sw.write_all(&fake_file[..5]).unwrap();
        sw.write_all(&fake_file[5..]).unwrap();
        mla.end_file(id).unwrap();

        // Using io::copy
        let id = mla.start_file("my_file2").unwrap();
        let mut sw = StreamWriter::new(&mut mla, id);
        assert_eq!(
            io::copy(&mut fake_file.as_slice(), &mut sw).unwrap(),
            fake_file.len() as u64
        );
        mla.end_file(id).unwrap();

        mla.finalize().unwrap();

        // Read the obtained stream
        let dest = mla.into_raw();
        let buf = Cursor::new(dest.as_slice());
        let mut mla_read = ArchiveReader::from_config(buf, ArchiveReaderConfig::new()).unwrap();
        let mut content1 = Vec::new();
        mla_read
            .get_file("my_file".to_string())
            .unwrap()
            .unwrap()
            .data
            .read_to_end(&mut content1)
            .unwrap();
        assert_eq!(content1.as_slice(), fake_file.as_slice());
        let mut content2 = Vec::new();
        mla_read
            .get_file("my_file2".to_string())
            .unwrap()
            .unwrap()
            .data
            .read_to_end(&mut content2)
            .unwrap();
        assert_eq!(content2.as_slice(), fake_file.as_slice());
    }
}
