//! Riconoscimento dei file trascinati e informazioni per la lista.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::pjl::Formato;

#[derive(Clone, Debug)]
pub struct InfoFile {
    pub formato: Formato,
    pub byte: u64,
    pub pagine: usize,
    /// Dimensioni della prima pagina in mm (per le immagini: supponendo 300 dpi).
    pub mm: Option<(f64, f64)>,
    /// Es. "3 pagine · 594 × 841 mm (A1)" oppure "4961 × 7016 px".
    pub dettagli: String,
}

/// Risoluzione presunta delle immagini senza DPI noti, per l'anteprima sul rotolo.
pub const DPI_IMMAGINI: f64 = 300.0;

pub fn riconosci(percorso: &Path) -> Result<Formato> {
    let mut testa = [0u8; 8];
    let letti = File::open(percorso)?.read(&mut testa)?;
    let testa = &testa[..letti];
    let est = percorso
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    Ok(if testa.starts_with(b"%PDF") {
        Formato::Pdf
    } else if testa.starts_with(b"%!") {
        Formato::PostScript
    } else if testa.starts_with(b"II*\0") || testa.starts_with(b"MM\0*") {
        Formato::Tiff
    } else if testa.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Formato::Jpeg
    } else if matches!(est.as_str(), "plt" | "hpgl" | "hpg" | "hp2" | "prn") {
        Formato::Hpgl2
    } else {
        bail!("formato non supportato (servono PDF, PS, TIFF, JPEG o HP-GL/2)")
    })
}

pub fn analizza(percorso: &Path) -> Result<InfoFile> {
    let formato = riconosci(percorso)?;
    let byte = std::fs::metadata(percorso)?.len();
    let mut info = InfoFile { formato, byte, pagine: 1, mm: None, dettagli: String::new() };
    match formato {
        Formato::Pdf => match leggi_pdf(percorso) {
            Ok((pagine, mm)) => {
                info.pagine = pagine;
                info.mm = mm;
                info.dettagli =
                    if pagine == 1 { "1 pagina".to_string() } else { format!("{pagine} pagine") };
                if let Some((l, a)) = mm {
                    info.dettagli.push_str(&format!(" · {l:.0} × {a:.0} mm{}", formato_iso(l, a)));
                }
            }
            Err(e) => info.dettagli = format!("PDF illeggibile: {e}"),
        },
        Formato::Tiff | Formato::Jpeg => {
            if let Ok(d) = imagesize::size(percorso) {
                let px_mm = 25.4 / DPI_IMMAGINI;
                info.mm = Some((d.width as f64 * px_mm, d.height as f64 * px_mm));
                info.dettagli = format!("{} × {} px", d.width, d.height);
            }
        }
        Formato::PostScript | Formato::Hpgl2 => {}
    }
    Ok(info)
}

/// Numero di pagine e dimensioni della prima pagina.
fn leggi_pdf(percorso: &Path) -> Result<(usize, Option<(f64, f64)>)> {
    let doc = lopdf::Document::load(percorso).context("apertura")?;
    let pagine = doc.get_pages();
    if pagine.is_empty() && doc.is_encrypted() {
        bail!("cifrato (serve la password)");
    }
    let mm = pagine.values().next().and_then(|&id| dimensioni_pagina_mm(&doc, id));
    Ok((pagine.len(), mm))
}

/// MediaBox e /Rotate (entrambi ereditabili) in mm, risalendo l'albero delle
/// pagine: vince il valore più vicino alla pagina.
fn dimensioni_pagina_mm(doc: &lopdf::Document, pagina: lopdf::ObjectId) -> Option<(f64, f64)> {
    let mut nodo = doc.get_dictionary(pagina).ok()?;
    let mut ruota = None;
    let mut lati = None;
    // Un albero di pagine è profondo pochi livelli: il limite evita di girare
    // all'infinito su un PDF con /Parent ciclici.
    for _ in 0..32 {
        if ruota.is_none() {
            ruota = nodo.get(b"Rotate").ok().and_then(|o| o.as_i64().ok());
        }
        if lati.is_none()
            && let Ok(mb) = nodo.get(b"MediaBox").and_then(|o| o.as_array())
        {
            let v: Vec<f64> =
                mb.iter().filter_map(|o| o.as_float().ok().map(f64::from)).collect();
            if v.len() != 4 {
                return None;
            }
            lati = Some(((v[2] - v[0]).abs(), (v[3] - v[1]).abs()));
        }
        if ruota.is_some() && lati.is_some() {
            break;
        }
        let Some(padre) = nodo.get(b"Parent").ok().and_then(|o| o.as_reference().ok()) else {
            break;
        };
        nodo = doc.get_dictionary(padre).ok()?;
    }
    let (l, a) = lati?;
    let pt_mm = 25.4 / 72.0;
    let (l, a) = (l * pt_mm, a * pt_mm);
    Some(if ruota.unwrap_or(0).rem_euclid(180) == 90 { (a, l) } else { (l, a) })
}

/// " (A1)" se le misure corrispondono a un formato ISO A, con 2 mm di tolleranza.
fn formato_iso(l: f64, a: f64) -> String {
    const ISO: [(&str, f64, f64); 6] = [
        ("A0", 841.0, 1189.0),
        ("A1", 594.0, 841.0),
        ("A2", 420.0, 594.0),
        ("A3", 297.0, 420.0),
        ("A4", 210.0, 297.0),
        ("A5", 148.0, 210.0),
    ];
    let (corto, lungo) = if l < a { (l, a) } else { (a, l) };
    ISO.iter()
        .find(|(_, c, g)| (corto - c).abs() < 2.0 && (lungo - g).abs() < 2.0)
        .map(|(n, ..)| format!(" ({n})"))
        .unwrap_or_default()
}

pub fn dimensione_leggibile(byte: u64) -> String {
    match byte {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::path::PathBuf;

    use lopdf::{Dictionary, Document, Object, Stream};

    // ---- file di prova ----

    fn cartella(nome: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("iqclock-file-{}-{nome}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scrivi(dir: &Path, nome: &str, dati: &[u8]) -> PathBuf {
        let p = dir.join(nome);
        std::fs::write(&p, dati).unwrap();
        p
    }

    /// Punti tipografici corrispondenti a `mm` millimetri.
    fn pt(mm: f64) -> f64 {
        mm * 72.0 / 25.4
    }

    #[derive(Clone, Copy)]
    struct PaginaTest {
        /// Larghezza e altezza del MediaBox in punti; `None` = ereditata.
        media_box: Option<(f64, f64)>,
        ruota: Option<i64>,
    }

    fn pagina(l: f64, a: f64) -> PaginaTest {
        PaginaTest { media_box: Some((l, a)), ruota: None }
    }

    fn media_box((l, a): (f64, f64)) -> Object {
        Object::Array([0.0, 0.0, l, a].iter().map(|v| Object::Real(*v as f32)).collect())
    }

    /// PDF minimo ma valido, con pagina o MediaBox ereditato dal nodo `Pages`.
    fn pdf(pagine: &[PaginaTest], media_box_padre: Option<(f64, f64)>) -> Vec<u8> {
        pdf_con_padre(pagine, media_box_padre, None)
    }

    fn pdf_con_padre(
        pagine: &[PaginaTest],
        media_box_padre: Option<(f64, f64)>,
        ruota_padre: Option<i64>,
    ) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let fogli_id = doc.new_object_id();
        let contenuto = doc.add_object(Stream::new(Dictionary::new(), Vec::new()));

        let mut fogli = Vec::new();
        for p in pagine {
            let mut d = Dictionary::new();
            d.set("Type", "Page");
            d.set("Parent", Object::Reference(fogli_id));
            d.set("Contents", Object::Reference(contenuto));
            d.set("Resources", Dictionary::new());
            if let Some(mb) = p.media_box {
                d.set("MediaBox", media_box(mb));
            }
            if let Some(r) = p.ruota {
                d.set("Rotate", r);
            }
            fogli.push(Object::Reference(doc.add_object(d)));
        }

        let mut albero = Dictionary::new();
        albero.set("Type", "Pages");
        albero.set("Kids", Object::Array(fogli));
        albero.set("Count", pagine.len() as i64);
        if let Some(mb) = media_box_padre {
            albero.set("MediaBox", media_box(mb));
        }
        if let Some(r) = ruota_padre {
            albero.set("Rotate", r);
        }
        doc.objects.insert(fogli_id, Object::Dictionary(albero));

        let mut catalogo = Dictionary::new();
        catalogo.set("Type", "Catalog");
        catalogo.set("Pages", Object::Reference(fogli_id));
        let catalogo_id = doc.add_object(catalogo);
        doc.trailer.set("Root", catalogo_id);

        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn analizza_bytes(dir: &Path, nome: &str, dati: &[u8]) -> InfoFile {
        analizza(&scrivi(dir, nome, dati)).unwrap()
    }

    // ---- riconosci ----

    #[test]
    fn riconosce_dai_byte_ignorando_l_estensione() {
        let dir = cartella("byte");
        let casi: [(&str, &[u8], Option<Formato>); 8] = [
            ("a.pdf", b"%PDF-1.7\n", Some(Formato::Pdf)),
            ("b.pdf", b"%!PS-Adobe-3.0", Some(Formato::PostScript)),
            ("c.pdf", b"II*\0xxxx", Some(Formato::Tiff)),
            ("d.pdf", b"MM\0*xxxx", Some(Formato::Tiff)),
            ("e.pdf", &[0xFF, 0xD8, 0xFF, 0xE0], Some(Formato::Jpeg)),
            ("f.plt", b"IN;SP1;", Some(Formato::Hpgl2)),
            ("g.txt", b"ciao", None),
            ("h.pdf", b"", None),
        ];
        for (nome, dati, atteso) in casi {
            let p = scrivi(&dir, nome, dati);
            assert_eq!(riconosci(&p).ok(), atteso, "{nome}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn riconosce_hpgl_dall_estensione() {
        let dir = cartella("hpgl");
        for est in ["plt", "hpgl", "hpg", "hp2", "prn", "PLT", "HpGl"] {
            let p = scrivi(&dir, &format!("disegno.{est}"), b"IN;SP1;");
            assert_eq!(riconosci(&p).ok(), Some(Formato::Hpgl2), "{est}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rifiuta_file_mancanti_e_png() {
        let dir = cartella("rifiuti");
        let mancante = dir.join("non-esiste.pdf");
        assert!(riconosci(&mancante).is_err());

        // Il supporto PNG è la issue #15: per ora deve essere rifiutato con un
        // messaggio che dica quali formati servono.
        let png = dir.join("foto.png");
        image::RgbImage::from_pixel(8, 8, image::Rgb([0, 0, 255]))
            .save_with_format(&png, image::ImageFormat::Png)
            .unwrap();
        let err = riconosci(&png).unwrap_err().to_string();
        assert!(err.contains("formato non supportato"), "{err}");
        assert!(analizza(&png).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---- PDF ----

    #[test]
    fn pdf_a4() {
        let dir = cartella("pdf-a4");
        let info = analizza_bytes(&dir, "a4.pdf", &pdf(&[pagina(pt(210.0), pt(297.0))], None));
        assert_eq!(info.formato, Formato::Pdf);
        assert_eq!(info.pagine, 1);
        assert_eq!(info.dettagli, "1 pagina · 210 × 297 mm (A4)");
        let (l, a) = info.mm.unwrap();
        assert!((l - 210.0).abs() < 0.5 && (a - 297.0).abs() < 0.5, "{l} × {a}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_formati_iso() {
        let dir = cartella("pdf-iso");
        let formati = [
            ("A0", 841.0, 1189.0),
            ("A1", 594.0, 841.0),
            ("A2", 420.0, 594.0),
            ("A3", 297.0, 420.0),
            ("A4", 210.0, 297.0),
            ("A5", 148.0, 210.0),
        ];
        for (nome, l, a) in formati {
            let p = scrivi(&dir, &format!("{nome}.pdf"), &pdf(&[pagina(pt(l), pt(a))], None));
            let info = analizza(&p).unwrap();
            assert_eq!(info.dettagli, format!("1 pagina · {l:.0} × {a:.0} mm ({nome})"), "{nome}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_formato_custom() {
        let dir = cartella("pdf-custom");
        let info = analizza_bytes(&dir, "custom.pdf", &pdf(&[pagina(800.0, 1200.0)], None));
        // 800 × 1200 pt = 282 × 423 mm: nessun formato ISO.
        assert_eq!(info.dettagli, "1 pagina · 282 × 423 mm");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_ruotato_scambia_larghezza_e_altezza() {
        let dir = cartella("pdf-ruotato");
        let mut p = pagina(pt(210.0), pt(297.0));
        p.ruota = Some(90);
        let info = analizza_bytes(&dir, "ruotato.pdf", &pdf(&[p], None));
        assert_eq!(info.dettagli, "1 pagina · 297 × 210 mm (A4)");

        // 180° non scambia le misure.
        let mut dritto = pagina(pt(210.0), pt(297.0));
        dritto.ruota = Some(180);
        let info = analizza_bytes(&dir, "capovolto.pdf", &pdf(&[dritto], None));
        assert_eq!(info.dettagli, "1 pagina · 210 × 297 mm (A4)");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_rotazione_ereditata_dal_padre() {
        let dir = cartella("pdf-rotazione-ereditata");
        let info = analizza_bytes(
            &dir,
            "padre90.pdf",
            &pdf_con_padre(&[pagina(pt(210.0), pt(297.0))], None, Some(90)),
        );
        assert_eq!(info.dettagli, "1 pagina · 297 × 210 mm (A4)");

        // La rotazione più vicina alla pagina ha la precedenza.
        let mut figlio = pagina(pt(210.0), pt(297.0));
        figlio.ruota = Some(180);
        let info = analizza_bytes(
            &dir,
            "figlio180.pdf",
            &pdf_con_padre(&[figlio], None, Some(90)),
        );
        assert_eq!(info.dettagli, "1 pagina · 210 × 297 mm (A4)");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_rotazione_e_media_box_entrambi_ereditati() {
        let dir = cartella("pdf-padre-completo");
        let senza_niente = PaginaTest { media_box: None, ruota: None };
        let info = analizza_bytes(
            &dir,
            "padre.pdf",
            &pdf_con_padre(&[senza_niente], Some((pt(210.0), pt(297.0))), Some(-90)),
        );
        assert_eq!(info.dettagli, "1 pagina · 297 × 210 mm (A4)");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_media_box_ereditato_dal_padre() {
        let dir = cartella("pdf-ereditato");
        let senza_media_box = PaginaTest { media_box: None, ruota: None };
        let info = analizza_bytes(
            &dir,
            "ereditato.pdf",
            &pdf(&[senza_media_box], Some((pt(594.0), pt(841.0)))),
        );
        assert_eq!(info.dettagli, "1 pagina · 594 × 841 mm (A1)");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_multipagina_usa_la_prima() {
        let dir = cartella("pdf-pagine");
        let pagine = [
            pagina(pt(594.0), pt(841.0)),
            pagina(pt(210.0), pt(297.0)),
            pagina(pt(420.0), pt(594.0)),
        ];
        let info = analizza_bytes(&dir, "tre.pdf", &pdf(&pagine, None));
        assert_eq!(info.pagine, 3);
        assert_eq!(info.dettagli, "3 pagine · 594 × 841 mm (A1)");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_senza_pagine() {
        let dir = cartella("pdf-vuoto");
        let info = analizza_bytes(&dir, "vuoto.pdf", &pdf(&[], None));
        assert_eq!(info.pagine, 0);
        assert_eq!(info.mm, None);
        assert_eq!(info.dettagli, "0 pagine");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_corrotto_finisce_nei_dettagli() {
        let dir = cartella("pdf-rotto");
        let info = analizza_bytes(&dir, "rotto.pdf", b"%PDF-1.7\nquesto non e un pdf");
        assert!(info.dettagli.starts_with("PDF illeggibile: "), "{}", info.dettagli);
        assert_eq!(info.pagine, 1);
        assert_eq!(info.mm, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pdf_cifrato_resta_leggibile_nella_lista() {
        let dir = cartella("pdf-cifrato");
        let mut doc = Document::load_mem(&pdf(&[pagina(pt(210.0), pt(297.0))], None)).unwrap();
        let mut cifratura = Dictionary::new();
        cifratura.set("Filter", "Standard");
        cifratura.set("V", 1);
        cifratura.set("R", 2);
        cifratura.set("O", Object::String(vec![0; 32], lopdf::StringFormat::Hexadecimal));
        cifratura.set("U", Object::String(vec![0; 32], lopdf::StringFormat::Hexadecimal));
        cifratura.set("P", -4);
        let cifratura = doc.add_object(cifratura);
        doc.trailer.set("Encrypt", Object::Reference(cifratura));
        let mut dati = Vec::new();
        doc.save_to(&mut dati).unwrap();

        let info = analizza_bytes(&dir, "cifrato.pdf", &dati);
        assert_eq!(info.formato, Formato::Pdf);
        // Senza password le pagine non sono leggibili: meglio dirlo che
        // mostrare "0 pagine".
        assert_eq!(info.dettagli, "PDF illeggibile: cifrato (serve la password)");
        assert_eq!(info.mm, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---- immagini e altri formati ----

    #[test]
    fn immagini_jpeg_e_tiff() {
        let dir = cartella("immagini");
        let rgb = image::RgbImage::from_pixel(40, 30, image::Rgb([10, 200, 10]));
        for (nome, formato, atteso) in [
            ("foto.jpg", image::ImageFormat::Jpeg, Formato::Jpeg),
            ("scansione.tif", image::ImageFormat::Tiff, Formato::Tiff),
        ] {
            let p = dir.join(nome);
            rgb.save_with_format(&p, formato).unwrap();
            assert_eq!(riconosci(&p).unwrap(), atteso, "{nome}");
            let info = analizza(&p).unwrap();
            assert_eq!(info.formato, atteso, "{nome}");
            assert_eq!(info.pagine, 1, "{nome}");
            assert_eq!(info.dettagli, "40 × 30 px", "{nome}");
            let (l, a) = info.mm.unwrap();
            // Senza DPI noti si assume DPI_IMMAGINI (300).
            assert!((l - 40.0 * 25.4 / DPI_IMMAGINI).abs() < 0.01, "{nome}: {l}");
            assert!((a - 30.0 * 25.4 / DPI_IMMAGINI).abs() < 0.01, "{nome}: {a}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn altre_estensioni_non_ingannano_il_riconoscimento() {
        let dir = cartella("estensioni");
        let rgb = image::RgbImage::from_pixel(4, 4, image::Rgb([0, 0, 0]));
        let jpeg = dir.join("finta.pdf");
        rgb.save_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap();
        assert_eq!(riconosci(&jpeg).unwrap(), Formato::Jpeg);

        let testo = scrivi(&dir, "finto.jpg", b"non sono un'immagine");
        assert!(riconosci(&testo).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hpgl_analizzato_senza_dimensioni() {
        let dir = cartella("hpgl-info");
        let info = analizza_bytes(&dir, "disegno.plt", b"IN;SP1;PA0,0;");
        assert_eq!(info.formato, Formato::Hpgl2);
        assert_eq!(info.pagine, 1);
        assert_eq!(info.mm, None);
        assert!(info.dettagli.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---- formato_iso ----

    #[test]
    fn iso_con_tolleranza() {
        for (nome, corto, lungo) in [
            ("A0", 841.0, 1189.0),
            ("A1", 594.0, 841.0),
            ("A2", 420.0, 594.0),
            ("A3", 297.0, 420.0),
            ("A4", 210.0, 297.0),
            ("A5", 148.0, 210.0),
        ] {
            assert_eq!(formato_iso(corto, lungo), format!(" ({nome})"));
            assert_eq!(formato_iso(lungo, corto), format!(" ({nome})"), "{nome} ruotato");
        }
    }

    #[test]
    fn iso_soglia_di_due_millimetri() {
        assert_eq!(formato_iso(594.3, 840.9), " (A1)");
        assert_eq!(formato_iso(595.9, 842.9), " (A1)");
        // Esattamente 2 mm: la tolleranza è esclusa.
        assert_eq!(formato_iso(596.0, 841.0), "");
        assert_eq!(formato_iso(594.0, 843.0), "");
        assert_eq!(formato_iso(600.0, 900.0), "");
        assert_eq!(formato_iso(0.0, 0.0), "");
    }
}
