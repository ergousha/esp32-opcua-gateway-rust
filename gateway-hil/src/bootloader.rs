//! Telling the project's second-stage bootloader from espflash's bundled one.
//!
//! Only the project's is built with `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`.
//! `espflash flash` writes its own unless given `--bootloader`, and that one
//! ignores OTA image states: a bad OTA image boot-loops on it instead of being
//! rolled back (issue #22). OTA never replaces the bootloader.
//!
//! The boot log cannot show which one runs: every attach resets the chip, the
//! USB-Serial-JTAG re-enumerates, and the log starts a second or more into the
//! boot, long after the bootloader's own lines. So the check reads the
//! bootloader back from flash instead.

/// Start of the partition table (`CONFIG_PARTITION_TABLE_OFFSET`); the
/// bootloader lives below it, from offset 0.
pub const REGION_LEN: u32 = 0x8000;

/// Image header: 8 bytes, then the 16-byte extended header.
const HEADER_LEN: usize = 24;
/// Byte 3 holds flash size and frequency, which espflash rewrites to the
/// flash settings it is given.
const FLASH_SETTINGS: usize = 3;
/// Last byte of the extended header: 1 when a SHA-256 of the image follows
/// it. espflash recomputes it after patching the header.
const HASH_APPENDED: usize = 23;
const HASH_LEN: usize = 32;

/// `esp_bootloader_desc_t`, the first thing in the first segment: magic
/// `0x50`, 3 reserved bytes, a `u32` version, `idf_ver[32]`, then
/// `date_time[24]`.
const DESC: usize = HEADER_LEN + 8;
const DESC_MAGIC: u8 = 0x50;
const IDF_VER: usize = DESC + 8;
const IDF_VER_LEN: usize = 32;
const DATE_TIME: std::ops::Range<usize> = IDF_VER + IDF_VER_LEN..IDF_VER + IDF_VER_LEN + 24;

/// The ESP-IDF version a bootloader image was built from, e.g. `v5.5.3`.
pub fn idf_version(image: &[u8]) -> Option<String> {
    if image.first() != Some(&0xE9) || image.get(DESC) != Some(&DESC_MAGIC) {
        return None;
    }
    let field = image.get(IDF_VER..IDF_VER + IDF_VER_LEN)?;
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    std::str::from_utf8(&field[..end]).ok().map(str::to_string)
}

/// True when `flashed` (read from offset 0) holds the same bootloader as
/// `built`: the same code and configuration, byte for byte, apart from
///
/// - the flash settings in the header, which espflash rewrites to the ones it
///   is given,
/// - the build timestamp in the descriptor, which differs between a debug and
///   a release build and across rebuilds of the same configuration,
/// - the checksum and the appended SHA-256, which cover both.
pub fn same_image(built: &[u8], flashed: &[u8]) -> bool {
    if built.len() <= DATE_TIME.end || flashed.len() < built.len() {
        return false;
    }
    let hashed = built[HASH_APPENDED] == 1;
    let end = built.len() - if hashed { HASH_LEN } else { 0 };
    let checksum = end - 1;
    let compared = |i: &usize| *i != FLASH_SETTINGS && !DATE_TIME.contains(i) && *i != checksum;
    hashed == (flashed[HASH_APPENDED] == 1)
        && (0..end).filter(compared).all(|i| built[i] == flashed[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal image: header, hash flag, bootloader descriptor, a segment
    /// body, and an appended hash.
    fn image(idf_ver: &str, body: u8) -> Vec<u8> {
        let mut image = vec![0u8; 96];
        image[0] = 0xE9;
        image[FLASH_SETTINGS] = 0x4F;
        image[HASH_APPENDED] = 1;
        image[DESC] = DESC_MAGIC;
        image[IDF_VER..IDF_VER + idf_ver.len()].copy_from_slice(idf_ver.as_bytes());
        image.extend([body; 64]);
        image.extend([0xAA; HASH_LEN]);
        image
    }

    #[test]
    fn reads_the_esp_idf_version() {
        assert_eq!(idf_version(&image("v5.5.3", 0)).as_deref(), Some("v5.5.3"));
        assert_eq!(
            idf_version(&image("v5.5.1-838-gd66ebb86d2e", 0)).as_deref(),
            Some("v5.5.1-838-gd66ebb86d2e")
        );
    }

    #[test]
    fn erased_or_foreign_flash_has_no_version() {
        assert_eq!(idf_version(&[0xFF; 128]), None);
        let mut no_desc = image("v5.5.3", 0);
        no_desc[DESC] = 0;
        assert_eq!(idf_version(&no_desc), None);
    }

    #[test]
    fn what_espflash_rewrites_is_ignored() {
        let built = image("v5.5.3", 7);
        let mut flashed = built.clone();
        flashed[FLASH_SETTINGS] = 0x40;
        let n = flashed.len();
        flashed[n - HASH_LEN..].fill(0x55);
        flashed[n - HASH_LEN - 1] ^= 0x0F;
        // The rest of the region below the partition table.
        flashed.extend([0xFF; 256]);
        assert!(same_image(&built, &flashed));
    }

    #[test]
    fn another_build_of_the_same_bootloader_matches() {
        let built = image("v5.5.3", 7);
        let mut rebuilt = built.clone();
        rebuilt[DATE_TIME].copy_from_slice(b"Nov  9 2026 14:12:31\0\0\0\0");
        assert!(same_image(&built, &rebuilt));
    }

    #[test]
    fn any_other_difference_is_a_different_image() {
        let built = image("v5.5.3", 7);
        assert!(!same_image(&built, &image("v5.5.3", 8)));
        assert!(!same_image(&built, &image("v5.5.1-838-gd66ebb86d2e", 7)));
        assert!(!same_image(&built, &built[..built.len() - 1]));
    }

    #[test]
    fn without_an_appended_hash_the_tail_counts() {
        let mut built = image("v5.5.3", 7);
        built[HASH_APPENDED] = 0;
        let mut flashed = built.clone();
        // Where the hash would be, if there were one.
        let n = flashed.len();
        flashed[n - HASH_LEN] ^= 1;
        assert!(!same_image(&built, &flashed));
    }
}
