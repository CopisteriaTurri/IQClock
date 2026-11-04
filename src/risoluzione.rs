//! Riscrittura della sola risoluzione dichiarata in TIFF e JPEG.
//!
//! Con una scala diversa dall'originale l'immagine non va ricampionata: cambiano
//! solo i DPI e la T2600 li rispetta (provato il 29/10/2026). Per JPEG e TIFF si
//! correggono quindi solo i campi della risoluzione, lasciando il resto del file
//! identico: un TIFF CMYK, in grigi, a 16 bit o con profilo ICC (e un JPEG CMYK)
//! resta quello che è, invece di passare dal decodificatore e uscire come TIFF
//! RGB 8 bit su bianco.
//!
//! Nei JPEG con JFIF ed EXIF diversi vince l'EXIF (provato, vedi
//! `file::dpi_jpeg`); qui si riscrive sempre anche il JFIF, così i due sono
//! coerenti e non si dipende da quella regola. Il JFIF dichiara densità intere:
//! con DPI non interi l'errore è meno di 0,5 dpi (428,57 → 429, 0,1 %). L'EXIF
//! invece è esatto.
//!
//! Quando la struttura non si sa riscrivere (`Ok(None)`: BigTIFF, file
//! troncato, densità JFIF fuori da `u16`), il chiamante ripiega sulla decodifica
//! come prima. Nei TIFF multipagina lo stesso fattore di scala si applica
//! ai DPI originali di ciascuna pagina.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::Result;

use crate::file::{self, FileTemporaneo, Ordine, leggi_a};
use crate::pjl::Formato;

/// Denominatore fisso dei RATIONAL scritti nel TIFF: 1/10000 di dpi.
const DENOMINATORE: u32 = 10_000;

/// Copia `sorgente` in un file temporaneo con la risoluzione dichiarata cambiata
/// in `dpi`. `Ok(None)` se il file ha una struttura che non sappiamo riscrivere
/// (il chiamante ripiega sulla decodifica, come oggi).
pub fn con_dpi(
    sorgente: &Path,
    formato: Formato,
    dpi: (f64, f64),
) -> Result<Option<FileTemporaneo>> {
    match formato {
        Formato::Tiff => {
            let temp = copia(sorgente, "tiff")?;
            let mut f = apri(&temp)?;
            let lunghezza = f.metadata()?.len();
            if riscrivi_risoluzione_tiff(&mut f, 0, lunghezza, dpi, true)? == EsitoTiff::Riscritto {
                Ok(Some(temp))
            } else {
                Ok(None)
            }
        }
        Formato::Jpeg => con_dpi_jpeg(sorgente, dpi),
        _ => Ok(None),
    }
}

fn copia(sorgente: &Path, estensione: &str) -> Result<FileTemporaneo> {
    let temp = FileTemporaneo::nuovo(estensione);
    std::fs::copy(sorgente, temp.percorso())?;
    Ok(temp)
}

fn apri(temp: &FileTemporaneo) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .open(temp.percorso())?)
}

fn con_dpi_jpeg(sorgente: &Path, dpi: (f64, f64)) -> Result<Option<FileTemporaneo>> {
    let temp = copia(sorgente, "jpg")?;
    let mut f = apri(&temp)?;
    let Some(ha_risoluzione) = riscrivi_jpeg(&mut f, dpi)? else {
        return Ok(None);
    };
    drop(f);
    if ha_risoluzione {
        return Ok(Some(temp));
    }
    // Senza JFIF né risoluzione nell'EXIF il segmento va aggiunto subito dopo
    // SOI: il resto del file si sposta, quindi si ricopia a blocchi.
    let Some(app0) = app0_jfif(dpi) else {
        return Ok(None);
    };
    let inserito = FileTemporaneo::nuovo("jpg");
    let mut sorgente = File::open(temp.percorso())?;
    let mut dest = File::create(inserito.percorso())?;
    dest.write_all(&[0xFF, 0xD8])?;
    dest.write_all(&app0)?;
    sorgente.seek(SeekFrom::Start(2))?;
    std::io::copy(&mut sorgente, &mut dest)?;
    dest.flush()?;
    Ok(Some(inserito))
}

/// Riscrive sul posto JFIF ed EXIF. `Some(true)` = il file ha ora una
/// risoluzione (basta), `Some(false)` = non ce n'era nessuna e va inserito un
/// APP0 JFIF, `None` = struttura non riscrivibile.
fn riscrivi_jpeg(f: &mut File, dpi: (f64, f64)) -> Result<Option<bool>> {
    let fine_file = f.metadata()?.len();
    let mut testa = [0u8; 2];
    if f.read_exact(&mut testa).is_err() || testa != [0xFF, 0xD8] {
        return Ok(None);
    }
    let (mut jfif, mut exif) = (None, false);
    while let Some((marcatore, inizio, lunghezza)) = file::prossimo_app_pos(f) {
        let fine = inizio + lunghezza;
        if fine > fine_file {
            return Ok(None); // file troncato
        }
        if marcatore == 0xE0 && jfif.is_none() && lunghezza >= 12 {
            let mut testa = [0u8; 12];
            f.seek(SeekFrom::Start(inizio))?;
            if f.read_exact(&mut testa).is_ok() && testa.starts_with(b"JFIF\0") {
                jfif = Some(inizio);
            }
        } else if marcatore == 0xE1 && !exif && lunghezza > 6 {
            let mut testa = [0u8; 6];
            f.seek(SeekFrom::Start(inizio))?;
            if f.read_exact(&mut testa).is_ok() && &testa == b"Exif\0\0" {
                let dati = leggi_a(f, inizio + 6, (lunghezza - 6) as usize);
                let leggibile = dati.as_ref().is_some_and(|dati| {
                    file::risoluzione_tiff(&mut std::io::Cursor::new(dati))
                        .and_then(|(x, y)| file::dpi_plausibili(x, y))
                        .is_some()
                });
                let esito = riscrivi_risoluzione_tiff(f, inizio + 6, lunghezza - 6, dpi, false)?;
                if esito == EsitoTiff::ConflittoRazionali
                    || (leggibile && esito != EsitoTiff::Riscritto)
                {
                    return Ok(None);
                }
                exif = esito == EsitoTiff::Riscritto;
            }
        }
        f.seek(SeekFrom::Start(fine))?;
    }
    if let Some(inizio) = jfif {
        // JFIF: unità in pollici e densità intere arrotondate.
        let (x, y) = match (densita(dpi.0), densita(dpi.1)) {
            (Some(x), Some(y)) => (x, y),
            _ => return Ok(None),
        };
        let mut valori = [1u8, 0, 0, 0, 0];
        valori[1..3].copy_from_slice(&x.to_be_bytes());
        valori[3..5].copy_from_slice(&y.to_be_bytes());
        f.seek(SeekFrom::Start(inizio + 7))?;
        f.write_all(&valori)?;
        Ok(Some(true))
    } else if exif {
        Ok(Some(true))
    } else if densita(dpi.0).is_some() && densita(dpi.1).is_some() {
        Ok(Some(false))
    } else {
        Ok(None)
    }
}

/// Densità JFIF: intera in `u16`, almeno 1.
fn densita(dpi: f64) -> Option<u16> {
    let densita = dpi.round();
    (densita >= 1.0 && densita <= f64::from(u16::MAX)).then_some(densita as u16)
}

/// Segmento APP0 JFIF di 18 byte: versione 1.02, pollici, niente miniatura.
fn app0_jfif(dpi: (f64, f64)) -> Option<[u8; 18]> {
    let (x, y) = (densita(dpi.0)?, densita(dpi.1)?);
    let mut b = [0u8; 18];
    b[0] = 0xFF;
    b[1] = 0xE0;
    b[2..4].copy_from_slice(&16u16.to_be_bytes());
    b[4..9].copy_from_slice(b"JFIF\0");
    b[9] = 1;
    b[10] = 2;
    b[11] = 1; // pollici
    b[12..14].copy_from_slice(&x.to_be_bytes());
    b[14..16].copy_from_slice(&y.to_be_bytes());
    Some(b)
}

#[derive(Debug, PartialEq, Eq)]
enum EsitoTiff {
    Riscritto,
    NonRiscrivibile,
    ConflittoRazionali,
}

/// Correzione dei tag di risoluzione di un TIFF che comincia a `base`.
/// Con `aggiungi` (solo per i TIFF veri, non per l'EXIF dentro un JPEG) un
/// IFD nuovo in fondo al file prende il posto di quello vecchio se X o Y mancano. Le scritture vengono preparate
/// solo dopo aver verificato tutti gli IFD, inclusi i razionali condivisi.
fn riscrivi_risoluzione_tiff(
    f: &mut File,
    base: u64,
    lunghezza: u64,
    dpi: (f64, f64),
    aggiungi: bool,
) -> Result<EsitoTiff> {
    Ok(riscrivi_tiff(f, base, lunghezza, dpi, aggiungi).unwrap_or(EsitoTiff::NonRiscrivibile))
}

/// Una voce dell'IFD: 12 byte nel file, qui i campi e i 4 byte del valore.
struct VoceTiff {
    tag: u16,
    tipo: u16,
    count: u32,
    valore: [u8; 4],
}

fn riscrivi_tiff(
    f: &mut File,
    base: u64,
    lunghezza: u64,
    dpi: (f64, f64),
    aggiungi: bool,
) -> Option<EsitoTiff> {
    let testa = leggi_a(f, base, 8)?;
    let ordine = match &testa[..2] {
        b"II" => Ordine::Piccolo,
        b"MM" => Ordine::Grande,
        _ => return None,
    };
    // 42 = TIFF classico; 43 è il BigTIFF, con offset a 64 bit: non lo scriviamo.
    if ordine.u16(&testa, 2)? != 42 {
        return None;
    }
    let ifd = u64::from(ordine.u32(&testa, 4)?);
    if ifd < 8 {
        return None;
    }
    // Legge e pianifica tutte le pagine prima di toccare il file: anche gli
    // offset condivisi fra IFD non devono alterare i DPI originali da leggere.
    let originali = if aggiungi {
        file::risoluzione_ifd(f, ordine, ifd)
            .and_then(|(x, y)| file::dpi_plausibili(x, y))
            .unwrap_or((300.0, 300.0))
    } else {
        dpi
    };
    let fattore = (dpi.0 / originali.0, dpi.1 / originali.1);
    let mut ifd = ifd;
    let mut puntatore = base + 4;
    let mut visitati = std::collections::HashSet::new();
    let mut scritture = Vec::new();
    let mut fondo = base.checked_add(lunghezza)?;
    while ifd != 0 {
        if ifd < 8 || ifd + 2 > lunghezza || !visitati.insert(ifd) {
            return None;
        }
        let originali = if aggiungi {
            file::risoluzione_ifd(f, ordine, ifd)
                .and_then(|(x, y)| file::dpi_plausibili(x, y))
                .unwrap_or((300.0, 300.0))
        } else {
            (0.0, 0.0)
        };
        let dpi_pagina = if aggiungi {
            (originali.0 * fattore.0, originali.1 * fattore.1)
        } else {
            dpi
        };
        let quante = usize::from(ordine.u16(&leggi_a(f, base + ifd, 2)?, 0)?);
        let fine_ifd = ifd + 2 + quante as u64 * 12;
        if fine_ifd + 4 > lunghezza {
            return None;
        }
        let prossimo = ordine.u32(&leggi_a(f, base + fine_ifd, 4)?, 0)?;
        let tabella = leggi_a(f, base + ifd + 2, quante * 12)?;
        let mut elenco = Vec::with_capacity(quante + 2);
        for voce in tabella.as_chunks::<12>().0 {
            elenco.push(VoceTiff {
                tag: ordine.u16(voce, 0)?,
                tipo: ordine.u16(voce, 2)?,
                count: ordine.u32(voce, 4)?,
                valore: voce[8..12].try_into().ok()?,
            });
        }
        let pagina = pianifica_ifd(
            ordine,
            base,
            lunghezza,
            dpi_pagina,
            aggiungi,
            ifd,
            prossimo,
            puntatore,
            &mut fondo,
            elenco,
            &mut scritture,
        )?;
        match pagina {
            Ok(successivo) => puntatore = successivo,
            Err(esito) => return Some(esito),
        }
        if !aggiungi {
            break; // EXIF: la risoluzione è nell'IFD0, non nelle miniature.
        }
        ifd = u64::from(prossimo);
    }
    for (posizione, dati) in scritture {
        f.seek(SeekFrom::Start(posizione)).ok()?;
        f.write_all(&dati).ok()?;
    }
    Some(EsitoTiff::Riscritto)
}

#[allow(clippy::too_many_arguments)]
fn pianifica_ifd(
    ordine: Ordine,
    base: u64,
    lunghezza: u64,
    dpi: (f64, f64),
    aggiungi: bool,
    ifd: u64,
    prossimo: u32,
    puntatore: u64,
    fondo: &mut u64,
    mut elenco: Vec<VoceTiff>,
    scritture: &mut Vec<(u64, Vec<u8>)>,
) -> Option<std::result::Result<u64, EsitoTiff>> {
    // Offset dei due RATIONAL, relativi all'inizio del TIFF: un valore di 8 byte
    // non sta nella voce, quindi la voce punta a dove sta davvero. Conta solo
    // se i suoi 8 byte stanno davvero dentro il TIFF/EXIF: un offset costruito
    // ad arte non deve far scrivere fuori dal file (o allungare il temporaneo
    // fino ai 4 GB di un offset a 32 bit).
    let razionale_valido = |v: &VoceTiff| {
        v.tipo == 5
            && v.count == 1
            && ordine.u32(&v.valore, 0).is_some_and(|offset| {
                let offset = u64::from(offset);
                offset >= 8 && offset + 8 <= lunghezza
            })
    };
    let (mut x, mut y, mut unita) = (None, None, None);
    for (i, voce) in elenco.iter().enumerate() {
        if voce.tag == 282 && razionale_valido(voce) {
            x = Some(u64::from(ordine.u32(&voce.valore, 0)?));
        } else if voce.tag == 283 && razionale_valido(voce) {
            y = Some(u64::from(ordine.u32(&voce.valore, 0)?));
        } else if voce.tag == 296 && voce.tipo == 3 && voce.count == 1 {
            unita = Some(i);
        }
    }
    let completa = x.is_some() && y.is_some();
    if !completa && !aggiungi {
        return Some(Err(EsitoTiff::NonRiscrivibile));
    }

    if x.is_some() && x == y && numeratore(dpi.0)? != numeratore(dpi.1)? {
        return Some(Err(EsitoTiff::ConflittoRazionali));
    }
    for (offset, dpi) in [(x, dpi.0), (y, dpi.1)] {
        if let Some(offset) = offset {
            let dati = razionale(ordine, dpi)?;
            // Lo stesso RATIONAL può essere condiviso anche fra pagine.
            if scritture
                .iter()
                .any(|(p, d)| *p == base + offset && *d != dati)
            {
                return Some(Err(EsitoTiff::ConflittoRazionali));
            }
            scritture.push((base + offset, dati));
        }
    }
    // ResolutionUnit diversa da 2 (3 = centimetri, 1 = nessuna unità
    // assoluta): si dichiara in pollici, come i valori scritti, altrimenti i
    // nuovi DPI non si leggono più (`file::risoluzione_tiff` scarta unità
    // relative). Se la voce manca vale già 2 per la specifica del TIFF.
    if let Some(i) = unita
        && ordine.u16(&elenco[i].valore, 0)? != 2
    {
        ordine.scrivi_u16(&mut elenco[i].valore, 0, 2);
        if completa {
            scritture.push((
                base + ifd + 2 + (i as u64) * 12 + 8,
                elenco[i].valore.to_vec(),
            ));
        }
    }
    if completa {
        return Some(Ok(base + ifd + 2 + elenco.len() as u64 * 12));
    }

    // Voci di X/Y che c'erano con un tipo strano: si rimpiazzano con quelle
    // nuove invece di duplicarle.
    if x.is_none() {
        elenco.retain(|v| v.tag != 282);
    }
    if y.is_none() {
        elenco.retain(|v| v.tag != 283);
    }
    let mut mancanti = Vec::new();
    if x.is_none() {
        mancanti.push((282u16, dpi.0));
    }
    if y.is_none() {
        mancanti.push((283u16, dpi.1));
    }

    // IFD nuovo in fondo al file, allineato a un offset pari, i due RATIONAL
    // subito dopo e lo stesso puntatore all'IFD successivo. Gli offset delle
    // altre voci restano validi perché il resto del file non si sposta.
    let inizio = if (*fondo - base).is_multiple_of(2) {
        *fondo
    } else {
        *fondo + 1
    };
    let razionali = inizio + 2 + ((elenco.len() + mancanti.len()) as u64) * 12 + 4;
    for (i, (tag, _)) in mancanti.iter().enumerate() {
        let offset = u32::try_from(razionali - base + 8 * i as u64).ok()?;
        let mut valore = [0u8; 4];
        ordine.scrivi_u32(&mut valore, 0, offset);
        elenco.push(VoceTiff {
            tag: *tag,
            tipo: 5,
            count: 1,
            valore,
        });
    }
    // La specifica vuole le voci in ordine di tag.
    elenco.sort_by_key(|v| v.tag);

    let mut fuori = Vec::with_capacity(2 + elenco.len() * 12 + 4 + mancanti.len() * 8);
    let mut quante_nuove = [0u8; 2];
    ordine.scrivi_u16(&mut quante_nuove, 0, u16::try_from(elenco.len()).ok()?);
    fuori.extend_from_slice(&quante_nuove);
    for voce in &elenco {
        let mut e = [0u8; 12];
        ordine.scrivi_u16(&mut e, 0, voce.tag);
        ordine.scrivi_u16(&mut e, 2, voce.tipo);
        ordine.scrivi_u32(&mut e, 4, voce.count);
        e[8..12].copy_from_slice(&voce.valore);
        fuori.extend_from_slice(&e);
    }
    let mut successivo = [0u8; 4];
    ordine.scrivi_u32(&mut successivo, 0, prossimo);
    fuori.extend_from_slice(&successivo);
    for (_, dpi) in &mancanti {
        let mut r = [0u8; 8];
        ordine.scrivi_u32(&mut r, 0, numeratore(*dpi)?);
        ordine.scrivi_u32(&mut r, 4, DENOMINATORE);
        fuori.extend_from_slice(&r);
    }
    let successivo = inizio + 2 + elenco.len() as u64 * 12;
    *fondo = inizio + fuori.len() as u64;
    scritture.push((inizio, fuori));
    // Il puntatore alla pagina (nell'header o nell'IFD precedente) cambia
    // senza spostare i pixel né gli altri tag.
    let mut nuovo = [0u8; 4];
    ordine.scrivi_u32(&mut nuovo, 0, u32::try_from(inizio - base).ok()?);
    scritture.push((puntatore, nuovo.to_vec()));
    Some(Ok(successivo))
}

/// DPI × 10000, arrotondato: il numeratore del RATIONAL.
fn numeratore(dpi: f64) -> Option<u32> {
    let n = (dpi * f64::from(DENOMINATORE)).round();
    (n >= 1.0 && n <= f64::from(u32::MAX)).then_some(n as u32)
}

fn razionale(ordine: Ordine, dpi: f64) -> Option<Vec<u8>> {
    let mut b = [0u8; 8];
    ordine.scrivi_u32(&mut b, 0, numeratore(dpi)?);
    ordine.scrivi_u32(&mut b, 4, DENOMINATORE);
    Some(b.to_vec())
}

#[cfg(test)]
mod test {
    use super::*;
    use std::path::PathBuf;

    fn cartella(nome: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "iqclock-risoluzione-{}-{nome}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scrivi(dir: &Path, nome: &str, dati: &[u8]) -> PathBuf {
        let p = dir.join(nome);
        std::fs::write(&p, dati).unwrap();
        p
    }

    /// Byte che differiscono tra due file della stessa lunghezza.
    fn differenze(a: &[u8], b: &[u8]) -> Vec<usize> {
        assert_eq!(a.len(), b.len());
        (0..a.len()).filter(|&i| a[i] != b[i]).collect()
    }

    /// Scrittore di numeri con l'ordine dei byte del TIFF.
    struct Bytes {
        grande: bool,
        v: Vec<u8>,
    }

    impl Bytes {
        fn nuovo(grande: bool) -> Self {
            Self {
                grande,
                v: Vec::new(),
            }
        }

        fn intestazione(&mut self, ifd: u32) {
            self.v
                .extend_from_slice(if self.grande { b"MM" } else { b"II" });
            self.u16(42);
            self.u32(ifd);
        }

        fn u16(&mut self, n: u16) {
            let b = if self.grande {
                n.to_be_bytes()
            } else {
                n.to_le_bytes()
            };
            self.v.extend_from_slice(&b);
        }

        fn u32(&mut self, n: u32) {
            let b = if self.grande {
                n.to_be_bytes()
            } else {
                n.to_le_bytes()
            };
            self.v.extend_from_slice(&b);
        }

        /// Voce dell'IFD; per gli SHORT il valore sta nei primi due byte.
        fn voce(&mut self, tag: u16, tipo: u16, count: u32, valore: u32) {
            self.u16(tag);
            self.u16(tipo);
            self.u32(count);
            if tipo == 3 {
                self.u16(valore as u16);
                self.u16(0);
            } else {
                self.u32(valore);
            }
        }
    }

    /// TIFF minimo con XResolution/YResolution (300 dpi) e ResolutionUnit.
    fn tiff_con_risoluzione(grande: bool, dpi: u32, unita: u16) -> Vec<u8> {
        let mut b = Bytes::nuovo(grande);
        b.intestazione(8);
        b.u16(5);
        b.voce(256, 3, 1, 100); // ImageWidth
        b.voce(257, 3, 1, 50); // ImageLength
        b.voce(282, 5, 1, 74); // XResolution: RATIONAL a 74
        b.voce(283, 5, 1, 82); // YResolution: RATIONAL a 82
        b.voce(296, 3, 1, u32::from(unita));
        b.u32(0); // nessun IFD successivo
        b.u32(dpi);
        b.u32(1);
        b.u32(dpi);
        b.u32(1);
        b.v
    }

    /// TIFF a 8 bit in grigi senza risoluzione, decodificabile: 6 × 4 pixel
    /// con i valori 0..24, IFD dopo i dati.
    fn tiff_grigio_senza_risoluzione(grande: bool) -> Vec<u8> {
        let (l, a) = (6u32, 4u32);
        let pixel: Vec<u8> = (0..(l * a) as u8).collect();
        let ifd = 8 + pixel.len() as u32;
        let mut b = Bytes::nuovo(grande);
        b.intestazione(ifd);
        b.v.extend_from_slice(&pixel);
        b.u16(10);
        b.voce(256, 3, 1, l);
        b.voce(257, 3, 1, a);
        b.voce(258, 3, 1, 8); // BitsPerSample
        b.voce(259, 3, 1, 1); // senza compressione
        b.voce(262, 3, 1, 1); // scala di grigi
        b.voce(273, 4, 1, 8); // StripOffsets
        b.voce(277, 3, 1, 1); // SamplesPerPixel
        b.voce(278, 4, 1, a); // RowsPerStrip
        b.voce(279, 4, 1, pixel.len() as u32); // StripByteCounts
        b.voce(284, 3, 1, 1); // PlanarConfiguration
        b.u32(0);
        b.v
    }

    #[test]
    fn razionali_condivisi_richiedono_numeratori_uguali() {
        let dir = cartella("condivisi");
        for grande in [false, true] {
            let ordine = if grande {
                Ordine::Grande
            } else {
                Ordine::Piccolo
            };
            let mut dati = tiff_con_risoluzione(grande, 300, 2);
            ordine.scrivi_u32(&mut dati, 54, 74); // Y punta allo stesso RATIONAL di X.
            let p = scrivi(&dir, "condiviso.tif", &dati);
            let mut f = OpenOptions::new().read(true).write(true).open(&p).unwrap();
            assert_eq!(
                riscrivi_risoluzione_tiff(&mut f, 0, dati.len() as u64, (150.0, 75.0), true)
                    .unwrap(),
                EsitoTiff::ConflittoRazionali
            );
            assert_eq!(
                std::fs::read(&p).unwrap(),
                dati,
                "nessuna scrittura prima del conflitto"
            );
            assert!(con_dpi(&p, Formato::Tiff, (150.0, 75.0)).unwrap().is_none());
            let temp = con_dpi(&p, Formato::Tiff, (150.0, 150.0)).unwrap().unwrap();
            assert_eq!(
                file::dpi_immagine(temp.percorso(), Formato::Tiff),
                (150.0, 150.0)
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tiff_multipagina_scala_i_dpi_di_ogni_pagina() {
        let dir = cartella("multipagina");
        for grande in [false, true] {
            let ordine = if grande {
                Ordine::Grande
            } else {
                Ordine::Piccolo
            };
            // Anche le pagine senza risoluzione richiedono un nuovo IFD:
            // controlla il collegamento sia prima sia dopo la sostituzione.
            for senza in [false, true] {
                let mut dati = if senza {
                    tiff_grigio_senza_risoluzione(grande)
                } else {
                    tiff_con_risoluzione(grande, 300, 2)
                };
                let prima_ifd = ordine.u32(&dati, 4).unwrap() as usize;
                let quante = ordine.u16(&dati, prima_ifd).unwrap() as usize;
                let seconda_ifd = dati.len() as u32;
                ordine.scrivi_u32(&mut dati, prima_ifd + 2 + quante * 12, seconda_ifd);
                let mut seconda = tiff_con_risoluzione(grande, 200, 3);
                // La seconda pagina ha X/Y diversi e risoluzione in centimetri.
                ordine.scrivi_u32(&mut seconda, 82, 100);
                for pos in [42, 54] {
                    let offset = ordine.u32(&seconda, pos).unwrap();
                    ordine.scrivi_u32(&mut seconda, pos, offset + seconda_ifd - 8);
                }
                dati.extend_from_slice(&seconda[8..]);
                let p = scrivi(&dir, "multi.tif", &dati);
                let temp = con_dpi(&p, Formato::Tiff, (150.0, 75.0)).unwrap().unwrap();
                let mut f = File::open(temp.percorso()).unwrap();
                let header = leggi_a(&mut f, 0, 8).unwrap();
                let ifd = u64::from(ordine.u32(&header, 4).unwrap());
                assert_eq!(
                    file::risoluzione_ifd(&mut f, ordine, ifd),
                    Some((150.0, 75.0))
                );
                let n = ordine.u16(&leggi_a(&mut f, ifd, 2).unwrap(), 0).unwrap();
                let prossimo = ordine
                    .u32(&leggi_a(&mut f, ifd + 2 + u64::from(n) * 12, 4).unwrap(), 0)
                    .unwrap();
                assert_eq!(
                    file::risoluzione_ifd(&mut f, ordine, u64::from(prossimo)),
                    Some((254.0, 63.5))
                );
                assert_eq!(std::fs::read(&p).unwrap(), dati);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn catena_ifd_ciclica_ripiega_sulla_decodifica() {
        let dir = cartella("ciclo");
        let mut dati = tiff_con_risoluzione(false, 300, 2);
        Ordine::Piccolo.scrivi_u32(&mut dati, 70, 8);
        let p = scrivi(&dir, "ciclo.tif", &dati);
        assert!(
            con_dpi(&p, Formato::Tiff, (150.0, 150.0))
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tiff_ii_e_mm_cambiano_solo_i_razionali() {
        for grande in [false, true] {
            let dir = cartella("tiff-ordine");
            let originale = tiff_con_risoluzione(grande, 300, 2);
            let p = scrivi(&dir, "orig.tif", &originale);
            let tmp = con_dpi(&p, Formato::Tiff, (150.0, 150.0))
                .unwrap()
                .expect("TIFF riscrivibile");
            let dopo = std::fs::read(tmp.percorso()).unwrap();
            // Le sole differenze stanno nei due RATIONAL, dopo la tabella
            // delle voci (8 byte di intestazione + 2 + 5 × 12 + 4); qualche
            // byte dei valori nuovi coincide con quello che c'era, quindi
            // quelli che differiscono sono al massimo 16.
            let inizio_razionali = 8 + 2 + 5 * 12 + 4;
            let cambi = differenze(&originale, &dopo);
            assert!(!cambi.is_empty() && cambi.len() <= 16, "{grande}: {cambi:?}");
            assert!(cambi.iter().all(|&i| i >= inizio_razionali), "{grande}");
            assert_eq!(
                file::dpi_immagine(tmp.percorso(), Formato::Tiff),
                (150.0, 150.0),
                "{grande}"
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn tiff_in_centimetri_torna_in_pollici() {
        let dir = cartella("tiff-cm");
        let p = scrivi(&dir, "cm.tif", &tiff_con_risoluzione(false, 118, 3));
        assert_eq!(
            file::dpi_immagine(&p, Formato::Tiff),
            (118.0 * 2.54, 118.0 * 2.54)
        );
        let tmp = con_dpi(&p, Formato::Tiff, (150.0, 150.0)).unwrap().unwrap();
        // 150 e non 381: l'unità è diventata "pollici" insieme ai valori.
        assert_eq!(
            file::dpi_immagine(tmp.percorso(), Formato::Tiff),
            (150.0, 150.0)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tiff_senza_unita_assoluta_diventa_pollici() {
        // Unità 1 = nessuna unità assoluta: i DPI non si leggono, quindi dopo
        // la riscrittura il file deve dichiarare i pollici.
        let dir = cartella("tiff-unita-1");
        let p = scrivi(&dir, "unita1.tif", &tiff_con_risoluzione(false, 300, 1));
        assert_eq!(
            file::dpi_immagine(&p, Formato::Tiff),
            (300.0, 300.0),
            "senza unità assoluta restano i DPI presunti"
        );
        let tmp = con_dpi(&p, Formato::Tiff, (150.0, 150.0)).unwrap().unwrap();
        assert_eq!(
            file::dpi_immagine(tmp.percorso(), Formato::Tiff),
            (150.0, 150.0)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn offset_fuori_dal_file_non_fa_crescere_il_temporaneo() {
        // XResolution punta oltre la fine del file: non si scrive lì, la voce
        // si rimpiazza con una sana in un IFD nuovo.
        let dir = cartella("offset-fuori");
        let mut b = Bytes::nuovo(false);
        b.intestazione(8);
        b.u16(5);
        b.voce(256, 3, 1, 100);
        b.voce(257, 3, 1, 50);
        b.voce(282, 5, 1, 0xFFFF_FFF0);
        b.voce(283, 5, 1, 82);
        b.voce(296, 3, 1, 2);
        b.u32(0);
        b.u32(150);
        b.u32(1);
        b.u32(150);
        b.u32(1);
        let originale = b.v;
        let p = scrivi(&dir, "fuori.tif", &originale);
        let tmp = con_dpi(&p, Formato::Tiff, (150.0, 150.0)).unwrap().unwrap();
        let dopo = std::fs::read(tmp.percorso()).unwrap();
        // Al massimo il nuovo IFD (voci + RATIONAL), non un file da 4 GB.
        assert!(
            dopo.len() <= originale.len() + 200,
            "il file è cresciuto da {} a {} byte",
            originale.len(),
            dopo.len()
        );
        assert_eq!(
            file::dpi_immagine(tmp.percorso(), Formato::Tiff),
            (150.0, 150.0)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tiff_senza_risoluzione_aggiunge_un_ifd_in_fondo() {
        use tiff::decoder::Decoder;
        for grande in [false, true] {
            let dir = cartella("tiff-senza");
            let originale = tiff_grigio_senza_risoluzione(grande);
            assert_eq!(file::dpi_immagine(&scrivi(&dir, "a.tif", &originale), Formato::Tiff), (300.0, 300.0));
            let p = scrivi(&dir, "orig.tif", &originale);
            let tmp = con_dpi(&p, Formato::Tiff, (150.0, 150.0)).unwrap().unwrap();
            let pixel = |decoder: &mut Decoder<File>| match decoder.read_image().unwrap() {
                tiff::decoder::DecodingResult::U8(v) => v,
                _ => panic!("pixel inattesi"),
            };
            let mut prima = Decoder::new(File::open(&p).unwrap()).unwrap();
            let pixel_prima = pixel(&mut prima);
            let mut dopo = Decoder::new(File::open(tmp.percorso()).unwrap()).unwrap();
            assert_eq!(dopo.dimensions().unwrap(), (6, 4), "{grande}");
            assert_eq!(pixel(&mut dopo), pixel_prima, "{grande}");
            assert_eq!(
                file::dpi_immagine(tmp.percorso(), Formato::Tiff),
                (150.0, 150.0),
                "{grande}"
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn bigtiff_e_troncati_non_si_riscrivono() {
        let dir = cartella("non-riscrivibili");
        let bigtiff = {
            let mut b = Bytes::nuovo(false);
            b.v.extend_from_slice(b"II");
            b.u16(43);
            b.u32(16);
            b.v.resize(64, 0);
            b.v
        };
        for (nome, dati) in [
            ("bigtiff.tif", bigtiff),
            (
                "troncato.tif",
                tiff_con_risoluzione(false, 300, 2)[..10].to_vec(),
            ),
            ("vuoto.tif", Vec::new()),
            ("non-tiff.tif", b"ciao, non sono un tiff".to_vec()),
        ] {
            let p = scrivi(&dir, nome, &dati);
            assert!(
                con_dpi(&p, Formato::Tiff, (150.0, 150.0))
                    .unwrap()
                    .is_none(),
                "{nome}"
            );
        }
        let non_jpeg = scrivi(&dir, "non.jpg", &tiff_con_risoluzione(false, 300, 2));
        assert!(
            con_dpi(&non_jpeg, Formato::Jpeg, (150.0, 150.0))
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jpeg_troncato_non_si_riscrive() {
        let dir = cartella("jpeg-troncato");
        let p = scrivi(
            &dir,
            "troncato.jpg",
            &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 1, 2, 3, 4],
        );
        assert!(
            con_dpi(&p, Formato::Jpeg, (150.0, 150.0))
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    // ---- JPEG ----

    fn jfif(unita: u8, x: u16, y: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xE0, 0x00, 0x10];
        v.extend_from_slice(b"JFIF\0\x01\x02");
        v.push(unita);
        v.extend_from_slice(&x.to_be_bytes());
        v.extend_from_slice(&y.to_be_bytes());
        v.extend_from_slice(&[0, 0]); // nessuna miniatura
        v
    }

    fn exif(dpi: u32) -> Vec<u8> {
        let tiff = tiff_con_risoluzione(false, dpi, 2);
        let mut v = vec![0xFF, 0xE1];
        v.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
        v.extend_from_slice(b"Exif\0\0");
        v.extend_from_slice(&tiff);
        v
    }

    /// Le densità del primo APP0 JFIF: (unità, x, y).
    fn jfif_letto(dati: &[u8]) -> Option<(u8, u16, u16)> {
        let mut c = std::io::Cursor::new(dati);
        c.set_position(2);
        while let Some((marcatore, inizio, lunghezza)) = file::prossimo_app_pos(&mut c) {
            if marcatore == 0xE0 && lunghezza >= 12 {
                let d = leggi_a(&mut c, inizio, 12)?;
                if d.starts_with(b"JFIF\0") {
                    return Some((
                        d[7],
                        u16::from_be_bytes([d[8], d[9]]),
                        u16::from_be_bytes([d[10], d[11]]),
                    ));
                }
            }
            c.set_position(inizio + lunghezza);
        }
        None
    }

    /// JPEG valido codificato da `image`, con i segmenti APP0/APP1 che
    /// scegliamo noi al posto di quelli del codificatore.
    fn jpeg_con(dir: &Path, nome: &str, segmenti: &[Vec<u8>]) -> PathBuf {
        let base = dir.join("base.jpg");
        image::RgbImage::from_fn(32, 24, |x, y| image::Rgb([x as u8, y as u8, 40]))
            .save_with_format(&base, image::ImageFormat::Jpeg)
            .unwrap();
        let dati = std::fs::read(&base).unwrap();
        let mut out = dati[..2].to_vec();
        for s in segmenti {
            out.extend_from_slice(s);
        }
        // Salta i segmenti APP0/APP1 del codificatore.
        let mut i = 2;
        while i + 4 <= dati.len() && dati[i] == 0xFF && matches!(dati[i + 1], 0xE0 | 0xE1) {
            let lunghezza = u16::from_be_bytes([dati[i + 2], dati[i + 3]]) as usize;
            i += 2 + lunghezza;
        }
        out.extend_from_slice(&dati[i..]);
        scrivi(dir, nome, &out)
    }

    /// Dopo la riscrittura: DPI nuovi e immagine che si decodifica ancora. Il
    /// file temporaneo viene cancellato all'uscita: si restituiscono i byte.
    fn verifica_jpeg(p: &Path, atteso: (f64, f64)) -> Vec<u8> {
        let tmp = con_dpi(p, Formato::Jpeg, atteso)
            .unwrap()
            .expect("JPEG riscrivibile");
        assert_eq!(file::dpi_immagine(tmp.percorso(), Formato::Jpeg), atteso);
        let letta = image::open(tmp.percorso()).unwrap().to_rgb8();
        assert_eq!(letta.dimensions(), (32, 24));
        std::fs::read(tmp.percorso()).unwrap()
    }

    #[test]
    fn jpeg_con_exif_leggibile_non_riscrivibile_ripiega_sulla_decodifica() {
        let dir = cartella("jpeg-exif-non-riscrivibile");
        for con_jfif in [false, true] {
            for condiviso in [false, true] {
                let mut app1 = exif(300);
                // L'EXIF parte a 10: SRATIONAL è leggibile ma non riscrivibile.
                if condiviso {
                    Ordine::Piccolo.scrivi_u32(&mut app1, 10 + 54, 74);
                } else {
                    Ordine::Piccolo.scrivi_u16(&mut app1, 10 + 36, 10);
                    Ordine::Piccolo.scrivi_u16(&mut app1, 10 + 48, 10);
                }
                let mut segmenti = vec![app1];
                if con_jfif {
                    segmenti.insert(0, jfif(1, 72, 72));
                }
                let p = jpeg_con(&dir, "non-riscrivibile.jpg", &segmenti);
                assert_eq!(file::dpi_immagine(&p, Formato::Jpeg), (300.0, 300.0));
                let prima = std::fs::read(&p).unwrap();
                let mut f = OpenOptions::new().read(true).write(true).open(&p).unwrap();
                assert_eq!(riscrivi_jpeg(&mut f, (150.0, 75.0)).unwrap(), None);
                assert_eq!(std::fs::read(&p).unwrap(), prima);
                assert!(con_dpi(&p, Formato::Jpeg, (150.0, 75.0)).unwrap().is_none());
                if condiviso {
                    verifica_jpeg(&p, (150.0, 150.0));
                }
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jpeg_con_solo_jfif() {
        let dir = cartella("jpeg-jfif");
        let p = jpeg_con(&dir, "jfif.jpg", &[jfif(1, 300, 300)]);
        verifica_jpeg(&p, (150.0, 150.0));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jpeg_con_solo_exif() {
        let dir = cartella("jpeg-exif");
        let p = jpeg_con(&dir, "exif.jpg", &[exif(300)]);
        verifica_jpeg(&p, (150.0, 150.0));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jpeg_con_entrambi_riscrive_anche_il_jfif() {
        let dir = cartella("jpeg-entrambi");
        let p = jpeg_con(&dir, "entrambi.jpg", &[jfif(1, 72, 72), exif(300)]);
        let dati = verifica_jpeg(&p, (150.0, 150.0));
        // L'EXIF vince, ma il JFIF deve dire la stessa cosa.
        assert_eq!(jfif_letto(&dati), Some((1, 150, 150)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jpeg_senza_risoluzione_riceve_un_app0() {
        let dir = cartella("jpeg-senza");
        let p = jpeg_con(&dir, "senza.jpg", &[]);
        assert_eq!(jfif_letto(&std::fs::read(&p).unwrap()), None);
        let dati = verifica_jpeg(&p, (150.0, 150.0));
        assert_eq!(jfif_letto(&dati), Some((1, 150, 150)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jpeg_con_dpi_non_interi_arrotonda_nel_jfif() {
        let dir = cartella("jpeg-frazioni");
        let p = jpeg_con(&dir, "frazioni.jpg", &[jfif(1, 300, 300)]);
        let tmp = con_dpi(&p, Formato::Jpeg, (428.57, 428.57)).unwrap().unwrap();
        assert_eq!(jfif_letto(&std::fs::read(tmp.percorso()).unwrap()), Some((1, 429, 429)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jpeg_con_dpi_fuori_dagli_interi_non_si_riscrive() {
        let dir = cartella("jpeg-fuori");
        let p = jpeg_con(&dir, "fuori.jpg", &[jfif(1, 300, 300)]);
        assert!(con_dpi(&p, Formato::Jpeg, (0.4, 150.0)).unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
