//! Conversione dei PNG in TIFF per l'invio alla stampante.
//!
//! La T2600 non legge i PNG sul canale raw (porta 9100): il file viene decodificato,
//! appiattito sul bianco della carta e riscritto in TIFF RGB 8 bit senza perdita
//! (LZW con predittore orizzontale). I DPI dichiarati nel chunk `pHYs` finiscono
//! nei tag di risoluzione del TIFF (`XResolution`/`YResolution`, `ResolutionUnit` =
//! inch); se il PNG non li dichiara si usano i 300 dpi presunti come nel resto
//! dell'app.
//!
//! Il TIFF viene scritto in un file temporaneo, cancellato quando il job è finito.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use image::DynamicImage;
use tiff::encoder::{Compression, Predictor, Rational, TiffEncoder, colortype};
use tiff::tags::ResolutionUnit;

use crate::file::DPI_IMMAGINI;

/// Firma di un file PNG (8 byte).
pub const FIRMA: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// DPI dichiarati nel chunk `pHYs`, se ci sono e sono sensati.
///
/// Il chunk dà i pixel per metro (unità 1); unità 0 vuol dire "rapporto
/// d'aspetto sconosciuto" e non serve a ricavare una dimensione di stampa.
pub fn dpi(percorso: &Path) -> Option<(f64, f64)> {
    let mut f = BufReader::new(File::open(percorso).ok()?);
    let mut firma = [0u8; 8];
    f.read_exact(&mut firma).ok()?;
    if &firma != FIRMA {
        return None;
    }
    loop {
        let mut testa = [0u8; 8];
        f.read_exact(&mut testa).ok()?;
        let lunghezza = u32::from_be_bytes(testa[..4].try_into().ok()?) as i64;
        match &testa[4..] {
            b"pHYs" => {
                let mut dati = [0u8; 9];
                f.read_exact(&mut dati).ok()?;
                let x = u32::from_be_bytes(dati[0..4].try_into().ok()?);
                let y = u32::from_be_bytes(dati[4..8].try_into().ok()?);
                if dati[8] != 1 || x == 0 || y == 0 {
                    return None;
                }
                let px_mm = 25.4 / 1000.0;
                return Some((x as f64 * px_mm, y as f64 * px_mm));
            }
            // Il pHYs deve venire prima dei dati immagine: se non l'abbiamo
            // ancora visto, non c'è.
            b"IDAT" | b"IEND" => return None,
            _ => {
                // Salta i dati del chunk e il suo CRC.
                f.seek(SeekFrom::Current(lunghezza + 4)).ok()?;
            }
        }
    }
}

/// TIFF temporaneo: il file viene cancellato quando questo valore esce di scena.
pub struct TiffTemporaneo {
    percorso: PathBuf,
}

impl TiffTemporaneo {
    pub fn percorso(&self) -> &Path {
        &self.percorso
    }
}

impl Drop for TiffTemporaneo {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.percorso);
    }
}

/// Converte il PNG in un TIFF RGB 8 bit su bianco, in un file temporaneo.
pub fn png_a_tiff(percorso: &Path) -> Result<TiffTemporaneo> {
    static CONTATORE: AtomicU64 = AtomicU64::new(0);
    // Senza limiti: quelli di default di `image` (512 MiB) bocciano già un A0 a 300 dpi.
    let mut lettore = image::ImageReader::open(percorso)?;
    lettore.no_limits();
    let immagine =
        lettore.decode().with_context(|| format!("lettura di {}", percorso.display()))?;
    let (larghezza, altezza, rgb) = su_bianco(immagine);
    let (dpi_x, dpi_y) = dpi(percorso).unwrap_or((DPI_IMMAGINI, DPI_IMMAGINI));

    let dest = std::env::temp_dir().join(format!(
        "iqclock-{}-{}.tiff",
        std::process::id(),
        CONTATORE.fetch_add(1, Ordering::Relaxed)
    ));
    let scrivi = || -> Result<()> {
        let mut tiff = TiffEncoder::new(BufWriter::new(File::create(&dest)?))?
            .with_compression(Compression::Lzw)
            .with_predictor(Predictor::Horizontal);
        let mut img = tiff.new_image::<colortype::RGB8>(larghezza, altezza)?;
        img.resolution_unit(ResolutionUnit::Inch);
        img.x_resolution(razionale(dpi_x));
        img.y_resolution(razionale(dpi_y));
        img.write_data(&rgb)?;
        Ok(())
    };
    if let Err(e) = scrivi() {
        let _ = std::fs::remove_file(&dest);
        return Err(e).context("conversione in TIFF");
    }
    Ok(TiffTemporaneo { percorso: dest })
}

/// RGB a 8 bit con l'eventuale trasparenza appiattita sul bianco della carta.
///
/// `into_rgba8` riduce anche i PNG a 16 bit a 8 bit per canale, che è quanto
/// serve alla stampa (e non copia l'immagine se è già RGBA a 8 bit).
fn su_bianco(immagine: DynamicImage) -> (u32, u32, Vec<u8>) {
    let rgba = immagine.into_rgba8();
    let (larghezza, altezza) = rgba.dimensions();
    let mut rgb = Vec::with_capacity((larghezza * altezza * 3) as usize);
    for p in rgba.pixels() {
        let [r, g, b, a] = p.0;
        let a = a as u32;
        let mescola = |c: u8| ((c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
        rgb.extend_from_slice(&[mescola(r), mescola(g), mescola(b)]);
    }
    (larghezza, altezza, rgb)
}

/// DPI → frazione razionale con tre decimali (il TIFF usa numeri razionali).
fn razionale(dpi: f64) -> Rational {
    let n = (dpi * 1000.0).round().clamp(1.0, u32::MAX as f64) as u32;
    Rational { n, d: 1000 }
}

#[cfg(test)]
mod test {
    use super::*;
    use image::{ImageBuffer, Rgb, RgbaImage};
    use tiff::decoder::Decoder;
    use tiff::decoder::ifd::Value;
    use tiff::tags::Tag;

    /// Risoluzione scritta nel TIFF, in dpi.
    fn dpi_scritti(percorso: &Path) -> (f64, f64) {
        let mut d = Decoder::new(File::open(percorso).unwrap()).unwrap();
        let razionale = |v: Value| match v {
            Value::Rational(n, c) => n as f64 / c as f64,
            v => panic!("risoluzione non razionale: {v:?}"),
        };
        let x = razionale(d.get_tag(Tag::XResolution).unwrap());
        let y = razionale(d.get_tag(Tag::YResolution).unwrap());
        (x, y)
    }

    fn cartella(nome: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("iqclock-png-{}-{nome}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// CRC-32 dei chunk PNG (ISO 3309).
    fn crc32(dati: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in dati {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }

    /// Inserisce un chunk `pHYs` subito dopo IHDR (come fa chi dichiara i DPI).
    fn con_phys(png: &[u8], pixel_per_metro: u32) -> Vec<u8> {
        let fine_ihdr = 8 + 4 + 4 + 13 + 4;
        assert_eq!(&png[12..16], b"IHDR");
        let mut corpo = b"pHYs".to_vec();
        corpo.extend_from_slice(&pixel_per_metro.to_be_bytes());
        corpo.extend_from_slice(&pixel_per_metro.to_be_bytes());
        corpo.push(1); // unità: pixel per metro
        let mut out = png[..fine_ihdr].to_vec();
        out.extend_from_slice(&9u32.to_be_bytes());
        out.extend_from_slice(&corpo);
        out.extend_from_slice(&crc32(&corpo).to_be_bytes());
        out.extend_from_slice(&png[fine_ihdr..]);
        out
    }

    #[test]
    fn dpi_da_phys() {
        let dir = cartella("phys");
        let p = dir.join("foto.png");
        let img = ImageBuffer::from_pixel(2, 1, Rgb([10u8, 20, 30]));
        img.save(&p).unwrap();
        assert_eq!(dpi(&p), None);

        // 11811 px/m ≈ 300 dpi, 5906 ≈ 150 dpi.
        std::fs::write(&p, con_phys(&std::fs::read(&p).unwrap(), 11811)).unwrap();
        let (x, y) = dpi(&p).unwrap();
        assert!((x - 300.0).abs() < 0.1 && (y - 300.0).abs() < 0.1, "{x} {y}");

        std::fs::write(&p, con_phys(&std::fs::read(&p).unwrap(), 5906)).unwrap();
        let (x, _) = dpi(&p).unwrap();
        assert!((x - 150.0).abs() < 0.1, "{x}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn converte_appiattendo_la_trasparenza() {
        let dir = cartella("trasparenza");
        let p = dir.join("logo.png");
        // Primo pixel trasparente, secondo blu pieno.
        let img = RgbaImage::from_raw(2, 1, vec![255u8, 0, 0, 0, 0, 0, 255, 255]).unwrap();
        img.save(&p).unwrap();

        let tiff = png_a_tiff(&p).unwrap();
        let rgb = image::open(tiff.percorso()).unwrap().to_rgb8();
        assert_eq!(rgb.dimensions(), (2, 1));
        assert_eq!(rgb.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(rgb.get_pixel(1, 0).0, [0, 0, 255]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn il_tiff_porta_i_dpi_del_png() {
        let dir = cartella("dpi");
        let p = dir.join("foto.png");
        ImageBuffer::from_pixel(4, 2, Rgb([1u8, 2, 3])).save(&p).unwrap();

        // Senza pHYs: 300 dpi presunti.
        let tiff = png_a_tiff(&p).unwrap();
        let mut d = Decoder::new(File::open(tiff.percorso()).unwrap()).unwrap();
        assert_eq!(d.dimensions().unwrap(), (4, 2));
        assert_eq!(d.get_tag_unsigned::<u16>(Tag::ResolutionUnit).unwrap(), 2); // inch
        assert!((dpi_scritti(tiff.percorso()).0 - 300.0).abs() < 0.01);
        let percorso = tiff.percorso().to_path_buf();
        drop(tiff);
        assert!(!percorso.exists(), "il TIFF temporaneo va cancellato");

        // Con pHYs a 150 dpi: quelli.
        std::fs::write(&p, con_phys(&std::fs::read(&p).unwrap(), 5906)).unwrap();
        let tiff = png_a_tiff(&p).unwrap();
        let (x, y) = dpi_scritti(tiff.percorso());
        assert!((x - 150.0).abs() < 0.1 && (y - 150.0).abs() < 0.1, "{x} {y}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
