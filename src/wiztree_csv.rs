use chrono::NaiveDateTime;
use flate2::{Compression, bufread::MultiGzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};

use std::io::{BufRead, Read, Write};

pub mod serde_quoted_string {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(data: &str, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Explicitly format with outer double quotes for WizTree
        format!("\"{data}\"").serialize(serializer)
    }

    #[expect(dead_code, reason = "we only special case serializing")]
    pub fn deserialize<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(s.trim_matches('"').to_string())
    }
}

pub mod serde_date_time {
    use chrono::NaiveDateTime;
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

    pub fn serialize<S>(data: &NaiveDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        data.format("%Y-%m-%d %H:%M:%S")
            .to_string()
            .serialize(serializer)
    }
    pub fn deserialize<'de, D>(deserializer: D) -> Result<NaiveDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = <&str>::deserialize(deserializer)?;
        let time =
            NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").map_err(de::Error::custom)?;
        Ok(time)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct WizTreeCsvRecord {
    #[serde(
        rename = "File Name",
        serialize_with = "serde_quoted_string::serialize"
    )]
    pub file_name: String,
    pub size: u64,
    pub allocated: u64,
    #[serde(with = "serde_date_time")]
    pub modified: NaiveDateTime,
    pub attributes: u64,
    pub files: u64,
    pub folders: u64,

    #[serde(default, rename = "DRIVECAPACITY")]
    pub drive_capacity: Option<u64>,
    #[serde(default, rename = "FREESPACE")]
    pub free_space: Option<u64>,
    #[serde(default, rename = "USEDSPACE")]
    pub used_space: Option<u64>,
    #[serde(default, rename = "RESERVEDSPACE")]
    pub reserved_space: Option<u64>,
}
impl WizTreeCsvRecord {
    /// Parse WizTree records from a Gzip compressed CSV.
    pub fn parse_compressed_csv<R: BufRead>(
        compressed_data: R,
    ) -> impl Iterator<Item = csv::Result<WizTreeCsvRecord>> {
        Self::parse_uncompressed_csv(MultiGzDecoder::new(compressed_data))
    }

    pub fn parse_uncompressed_csv<R>(
        reader: R,
    ) -> impl Iterator<Item = csv::Result<WizTreeCsvRecord>>
    where
        R: Read,
    {
        csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(reader)
            .into_deserialize()
    }

    pub fn write_csv_to<I, W>(
        records: I,
        writer: W,
    ) -> Result<W, Box<dyn std::error::Error + 'static>>
    where
        I: IntoIterator<Item = WizTreeCsvRecord>,
        W: Write,
    {
        let records = records.into_iter();
        let mut writer = csv::WriterBuilder::new()
            .flexible(true)
            .has_headers(true)
            .quote_style(csv::QuoteStyle::Never) // Disables automatic quoting for all fields
            .from_writer(writer);
        for record in records {
            writer.serialize(record)?;
        }
        let mut writer = writer.into_inner().map_err(|e| e.into_error())?;
        writer.flush()?;
        Ok(writer)
    }

    pub fn create_compressed_csv(
        records: impl IntoIterator<Item = WizTreeCsvRecord>,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + 'static>> {
        let mut gz_encoder = GzEncoder::new(Vec::new(), Compression::best());
        Self::write_csv_to(records, &mut gz_encoder)?;
        Ok(gz_encoder.finish()?)
    }
}
