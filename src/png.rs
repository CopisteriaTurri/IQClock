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
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result};
use image::{DynamicImage, RgbImage};
use tiff::encoder::Rational;
use tiff::encoder::compression::{CompressionAlgorithm, Lzw};

use crate::file::{DPI_IMMAGINI, FileTemporaneo, dpi_plausibili};

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
                if dati[8] != 1 {
                    return None;
                }
                let px_mm = 25.4 / 1000.0;
                return dpi_plausibili(x as f64 * px_mm, y as f64 * px_mm);
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

/// Converte il PNG in un TIFF RGB 8 bit su bianco, in un file temporaneo.
pub fn png_a_tiff(percorso: &Path) -> Result<FileTemporaneo> {
    let rgb = su_bianco(decodifica(percorso)?);
    let dpi = dpi(percorso).unwrap_or((DPI_IMMAGINI, DPI_IMMAGINI));
    let riga = rgb.width() as usize * 3;
    tiff_temporaneo(rgb.width(), rgb.height(), dpi, |y, out| {
        let inizio = y as usize * riga;
        out.copy_from_slice(&rgb.as_raw()[inizio..inizio + riga]);
    })
}

/// Decodifica un'immagine intera.
pub fn decodifica(percorso: &Path) -> Result<DynamicImage> {
    // Senza limiti: quelli di default di `image` (512 MiB) bocciano già un A0 a 300 dpi.
    let mut lettore = image::ImageReader::open(percorso)?.with_guessed_format()?;
    lettore.no_limits();
    lettore.decode().with_context(|| format!("lettura di {}", percorso.display()))
}

/// Scrive un TIFF RGB 8 bit (LZW con predittore) in un file temporaneo, una
/// striscia alla volta: `riga(y, out)` riempie la riga `y` (`larghezza` × 3 byte).
/// Così un'immagine allargata al rotolo non deve stare tutta in memoria.
///
/// Il TIFF lo scriviamo a mano: la crate `tiff` comprime solo con `write_data`,
/// che vuole l'immagine intera; le strisce scritte da sole uscirebbero non
/// compresse con il tag che dice LZW. Della crate si usa il compressore LZW.
pub fn tiff_temporaneo(
    larghezza: u32,
    altezza: u32,
    (dpi_x, dpi_y): (f64, f64),
    riga: impl FnMut(u32, &mut [u8]),
) -> Result<FileTemporaneo> {
    let dest = FileTemporaneo::nuovo("tiff");
    let mut out = BufWriter::new(File::create(dest.percorso())?);
    scrivi_tiff(&mut out, larghezza, altezza, (dpi_x, dpi_y), riga).context("scrittura del TIFF")?;
    out.flush()?;
    Ok(dest)
}

const RIGHE_PER_STRISCIA: u32 = 64;

fn scrivi_tiff<W: Write + Seek>(
    out: &mut W,
    larghezza: u32,
    altezza: u32,
    (dpi_x, dpi_y): (f64, f64),
    mut riga: impl FnMut(u32, &mut [u8]),
) -> Result<()> {
    // Intestazione little endian; l'offset della directory si scrive alla fine.
    out.write_all(b"II*\0\0\0\0\0")?;
    let mut posizione = 8u64;
    let byte_riga = larghezza as usize * 3;
    let mut offset_strisce = Vec::new();
    let mut byte_strisce = Vec::new();
    let mut striscia = Vec::new();
    for inizio in (0..altezza).step_by(RIGHE_PER_STRISCIA as usize) {
        let righe = RIGHE_PER_STRISCIA.min(altezza - inizio);
        striscia.resize(righe as usize * byte_riga, 0);
        for (y, r) in (inizio..).zip(striscia.chunks_exact_mut(byte_riga)) {
            riga(y, r);
            // Predittore orizzontale: ogni campione diventa la differenza col
            // pixel a sinistra (da destra verso sinistra, per non perderlo).
            for i in (3..r.len()).rev() {
                r[i] = r[i].wrapping_sub(r[i - 3]);
            }
        }
        let n = Lzw.write_to(out, &striscia)?;
        offset_strisce.push(u32::try_from(posizione).context("TIFF oltre 4 GB")?);
        byte_strisce.push(n as u32);
        posizione += n;
    }

    // Valori che non stanno nei 4 byte della voce: dopo i dati.
    let mut extra = Vec::new();
    let mut fuori = |dati: &[u8]| -> u32 {
        let offset = posizione + extra.len() as u64;
        extra.extend_from_slice(dati);
        offset as u32
    };
    let bit = fuori(&[8, 0, 8, 0, 8, 0]);
    let razionale = |dpi: f64| {
        let r = razionale(dpi);
        [r.n.to_le_bytes(), r.d.to_le_bytes()].concat()
    };
    let res_x = fuori(&razionale(dpi_x));
    let res_y = fuori(&razionale(dpi_y));
    let lunghi = |v: &[u32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
    let n_strisce = offset_strisce.len() as u32;
    let (offset, conteggi) = if n_strisce == 1 {
        (offset_strisce[0], byte_strisce[0])
    } else {
        (fuori(&lunghi(&offset_strisce)), fuori(&lunghi(&byte_strisce)))
    };
    if extra.len() % 2 == 1 {
        extra.push(0);
    }
    let directory = posizione + extra.len() as u64;
    const SHORT: u16 = 3;
    const LONG: u16 = 4;
    const RATIONAL: u16 = 5;
    // (tag, tipo, quanti, valore o offset), in ordine di tag come vuole il formato.
    let voci: [(u16, u16, u32, u32); 14] = [
        (256, LONG, 1, larghezza),
        (257, LONG, 1, altezza),
        (258, SHORT, 3, bit),
        (259, SHORT, 1, 5), // LZW
        (262, SHORT, 1, 2), // RGB
        (273, LONG, n_strisce, offset),
        (277, SHORT, 1, 3),
        (278, LONG, 1, RIGHE_PER_STRISCIA),
        (279, LONG, n_strisce, conteggi),
        (282, RATIONAL, 1, res_x),
        (283, RATIONAL, 1, res_y),
        (284, SHORT, 1, 1), // campioni interlacciati
        (296, SHORT, 1, 2), // pollici
        (317, SHORT, 1, 2), // predittore orizzontale
    ];
    out.write_all(&extra)?;
    out.write_all(&(voci.len() as u16).to_le_bytes())?;
    for (tag, tipo, quanti, valore) in voci {
        out.write_all(&tag.to_le_bytes())?;
        out.write_all(&tipo.to_le_bytes())?;
        out.write_all(&quanti.to_le_bytes())?;
        // Uno SHORT sta nei primi due byte del campo valore.
        out.write_all(&valore.to_le_bytes())?;
    }
    out.write_all(&0u32.to_le_bytes())?;
    out.seek(SeekFrom::Start(4))?;
    out.write_all(&u32::try_from(directory).context("TIFF oltre 4 GB")?.to_le_bytes())?;
    Ok(())
}

/// RGB a 8 bit con l'eventuale trasparenza appiattita sul bianco della carta.
///
/// `into_rgba8` riduce anche i PNG a 16 bit a 8 bit per canale, che è quanto
/// serve alla stampa (e non copia l'immagine se è già RGBA a 8 bit).
pub fn su_bianco(immagine: DynamicImage) -> RgbImage {
    let rgba = immagine.into_rgba8();
    let (larghezza, altezza) = rgba.dimensions();
    let mut rgb = Vec::with_capacity((larghezza * altezza * 3) as usize);
    for p in rgba.pixels() {
        let [r, g, b, a] = p.0;
        let a = a as u32;
        let mescola = |c: u8| ((c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
        rgb.extend_from_slice(&[mescola(r), mescola(g), mescola(b)]);
    }
    RgbImage::from_raw(larghezza, altezza, rgb).expect("dimensioni coerenti")
}

/// DPI → frazione razionale con tre decimali (il TIFF usa numeri razionali).
fn razionale(dpi: f64) -> Rational {
    let n = (dpi * 1000.0).round().clamp(1.0, u32::MAX as f64) as u32;
    Rational { n, d: 1000 }
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use std::path::PathBuf;
    use image::{ImageBuffer, Rgb, RgbImage, RgbaImage};
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
    pub(crate) fn con_phys(png: &[u8], pixel_per_metro: u32) -> Vec<u8> {
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

    #[test]
    fn tiff_a_piu_strisce_si_rilegge_identico() {
        // 3 strisce (64 + 64 + 2 righe) con pixel tutti diversi: verifica
        // predittore, LZW e tabella delle strisce.
        let (l, a) = (37, 130);
        let img = RgbImage::from_fn(l, a, |x, y| Rgb([(x * 7 + y) as u8, (y * 3) as u8, (x ^ y) as u8]));
        let tiff = tiff_temporaneo(l, a, (200.0, 100.0), |y, out| {
            let byte = l as usize * 3;
            out.copy_from_slice(&img.as_raw()[y as usize * byte..(y as usize + 1) * byte]);
        })
        .unwrap();
        let letta = image::open(tiff.percorso()).unwrap().to_rgb8();
        assert!(letta == img, "pixel diversi dopo la rilettura");
        let (x, y) = dpi_scritti(tiff.percorso());
        assert!((x - 200.0).abs() < 0.01 && (y - 100.0).abs() < 0.01, "{x} {y}");
    }
}
