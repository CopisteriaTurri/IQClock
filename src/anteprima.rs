//! Rendering delle anteprime (prime pagine dei PDF con pdfium, immagini con `image`)
//! su un thread dedicato: pdfium non è thread-safe, quindi una sola istanza lo usa.
//!
//! La libreria pdfium (`pdfium.dll` / `libpdfium.so`, build chromium/7881 di
//! bblanchon/pdfium-binaries) può stare accanto all'eseguibile; con la feature
//! `pdfium-incluso` (usata per le release) è dentro l'eseguibile e viene estratta
//! nella cache utente. Se manca, i PDF restano senza anteprima e il resto funziona.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use eframe::egui::ColorImage;
use pdfium_render::prelude::*;

use crate::file;
use crate::impagina::{self, Impaginazione};
use crate::lavorazione::{self, Lavorazione};
use crate::pjl::Formato;
use crate::png;

/// Pagine renderizzate per documento (quante ne mostra l'anteprima sul rotolo).
pub const PAGINE_MAX: usize = 4;
/// Lato lungo massimo delle immagini di anteprima, in pixel.
const LATO_MAX: i32 = 1400;

/// Una pagina renderizzata con le sue misure (le pagine di un PDF possono
/// avere formati diversi; per le immagini le misure si conoscono solo se
/// impaginate sul rotolo).
pub struct Pagina {
    pub immagine: ColorImage,
    pub mm: Option<(f64, f64)>,
    /// true se IQClock l'ha impaginata sul rotolo (pagina larga quanto la
    /// parte stampabile, che parte dal margine).
    pub impaginata: bool,
    /// true se IQClock ha ruotato la pagina di 90° per impaginarla.
    pub ruotata: bool,
}

struct Richiesta {
    id: u64,
    percorso: PathBuf,
    formato: Formato,
    lavorazione: Lavorazione,
    impaginazione: Option<Impaginazione>,
}

pub struct Anteprime {
    richieste: Sender<Richiesta>,
}

impl Anteprime {
    /// Avvia il thread. `consegna` riceve l'id del documento e le sue pagine.
    /// Restituisce anche l'errore di caricamento di pdfium, se c'è stato.
    pub fn avvia(consegna: impl Fn(u64, Vec<Pagina>) + Send + 'static) -> (Self, Option<String>) {
        let (richieste, rx) = channel::<Richiesta>();
        let (esito_tx, esito_rx) = channel();
        thread::spawn(move || {
            let pdfium = carica_pdfium();
            let _ = esito_tx.send(pdfium.as_ref().err().cloned());
            while let Ok(prima) = rx.recv() {
                // Cambiando allineamento o margine arrivano richieste a raffica:
                // per ogni documento conta solo l'ultima.
                let mut ultime: Vec<Richiesta> = Vec::new();
                for r in std::iter::once(prima).chain(rx.try_iter()) {
                    ultime.retain(|u| u.id != r.id);
                    ultime.push(r);
                }
                for r in ultime {
                    consegna(
                        r.id,
                        pagine_per_richiesta(
                            &pdfium,
                            &r.percorso,
                            r.formato,
                            &r.lavorazione,
                            r.impaginazione.as_ref(),
                        ),
                    );
                }
            }
        });
        let errore = esito_rx.recv().unwrap_or_else(|_| Some("thread anteprime terminato".into()));
        (Self { richieste }, errore)
    }

    /// L'anteprima passa da scala e impaginazione, cioè è il file che parte
    /// davvero.
    pub fn richiedi(
        &self,
        id: u64,
        percorso: PathBuf,
        formato: Formato,
        lavorazione: Lavorazione,
        impaginazione: Option<Impaginazione>,
    ) {
        let _ = self.richieste.send(Richiesta { id, percorso, formato, lavorazione, impaginazione });
    }
}

/// Una richiesta del thread anteprime: i PDF senza pdfium (o illeggibili)
/// danno zero pagine ma non bloccano il resto (le immagini funzionano comunque).
fn pagine_per_richiesta(
    pdfium: &Result<Pdfium, String>,
    percorso: &Path,
    formato: Formato,
    lav: &Lavorazione,
    imp: Option<&Impaginazione>,
) -> Vec<Pagina> {
    match formato {
        Formato::Pdf => match pdfium {
            Ok(p) => pagine_pdf(p, percorso, lav, imp).unwrap_or_default(),
            Err(_) => Vec::new(),
        },
        Formato::Jpeg | Formato::Tiff | Formato::Png => {
            immagine(percorso, formato, lav, imp).into_iter().collect()
        }
        Formato::PostScript | Formato::Hpgl2 => Vec::new(),
    }
}

/// Cerca pdfium accanto all'eseguibile, poi (in debug) in `pdfium/<piattaforma>`
/// del progetto, poi quello incluso nell'eseguibile, infine tra le librerie di sistema.
fn carica_pdfium() -> Result<Pdfium, String> {
    #[allow(unused_mut)]
    let mut cartelle = cartelle_pdfium();
    #[cfg(feature = "pdfium-incluso")]
    cartelle.push(estrai_pdfium().map_err(|e| format!("estrazione di pdfium non riuscita: {e}"))?);
    carica_pdfium_in(&cartelle, true)
}

fn cartelle_pdfium() -> Vec<PathBuf> {
    let mut cartelle: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        cartelle.push(dir);
    }
    if cfg!(debug_assertions) {
        cartelle.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("pdfium").join(PIATTAFORMA));
    }
    cartelle
}

/// Variante testabile di [`carica_pdfium`]: cerca solo in `cartelle` e, se
/// `con_fallback_sistema` è falso, non prova le librerie di sistema (così il
/// caso "libpdfium mancante" è riproducibile anche dove pdfium è installato).
fn carica_pdfium_in(cartelle: &[PathBuf], con_fallback_sistema: bool) -> Result<Pdfium, String> {
    for dir in cartelle {
        let lib = Pdfium::pdfium_platform_library_name_at_path(dir);
        if lib.exists() {
            return match Pdfium::bind_to_library(&lib) {
                Ok(bindings) => Ok(Pdfium::new(bindings)),
                // pdfium-render consente un solo binding per processo: nei test in
                // parallelo (ma non nell'app, che lo carica una volta sola) la
                // libreria può essere già inizializzata. `Pdfium::default()` in quel
                // caso riusa i binding esistenti invece di riaprire la libreria.
                Err(PdfiumError::PdfiumLibraryBindingsAlreadyInitialized) => Ok(Pdfium::default()),
                Err(e) => Err(format!("{}: {e}", lib.display())),
            };
        }
    }
    if con_fallback_sistema {
        return match Pdfium::bind_to_system_library() {
            Ok(bindings) => Ok(Pdfium::new(bindings)),
            Err(PdfiumError::PdfiumLibraryBindingsAlreadyInitialized) => Ok(Pdfium::default()),
            Err(_) => Err(messaggio_pdfium_mancante()),
        };
    }
    Err(messaggio_pdfium_mancante())
}

fn messaggio_pdfium_mancante() -> String {
    format!(
        "{} non trovata accanto all'eseguibile: anteprima dei PDF non disponibile",
        Pdfium::pdfium_platform_library_name().to_string_lossy()
    )
}

const PIATTAFORMA: &str = if cfg!(windows) { "win-x64" } else { "linux-x64" };

#[cfg(all(feature = "pdfium-incluso", windows))]
const PDFIUM_INCLUSO: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/pdfium/win-x64/pdfium.dll"));
#[cfg(all(feature = "pdfium-incluso", not(windows)))]
const PDFIUM_INCLUSO: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/pdfium/linux-x64/libpdfium.so"));

/// Scrive la pdfium inclusa nella cache utente (`%LOCALAPPDATA%\iqclock` su Windows,
/// `~/.cache/iqclock` altrove), se non c'è già identica, e ne restituisce la cartella.
#[cfg(feature = "pdfium-incluso")]
fn estrai_pdfium() -> std::io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        })
        .ok_or_else(|| std::io::Error::other("cartella di cache non trovata"))?;
    let dir = base.join("iqclock");
    let lib = Pdfium::pdfium_platform_library_name_at_path(&dir);
    if std::fs::read(&lib).ok().as_deref() != Some(PDFIUM_INCLUSO) {
        std::fs::create_dir_all(&dir)?;
        // Prima su un file a parte: due istanze avviate insieme non vedono mai
        // una libreria scritta a metà.
        let tmp = dir.join(format!("pdfium-{}.tmp", std::process::id()));
        std::fs::write(&tmp, PDFIUM_INCLUSO)?;
        std::fs::rename(&tmp, &lib).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
    }
    Ok(dir)
}

fn pagine_pdf(
    pdfium: &Pdfium,
    percorso: &Path,
    lav: &Lavorazione,
    imp: Option<&Impaginazione>,
) -> Result<Vec<Pagina>, PdfiumError> {
    // Lo stesso PDF (scalato e impaginato) che `stampante.rs` manderebbe; se non
    // riesce, l'errore lo darà la stampa e qui resta la pagina originale.
    let composto = if lav.nulla() && imp.is_none() {
        None
    } else {
        // Se la lettura o la composizione falliscono resta la pagina originale:
        // l'errore vero lo darà la stampa.
        std::fs::read(percorso)
            .ok()
            .and_then(|dati| lavorazione::componi_pdf(&dati, lav, imp).ok().flatten())
    };
    let (doc, posizioni) = match &composto {
        Some(c) => (pdfium.load_pdf_from_byte_slice(&c.dati, None)?, c.posizioni.as_slice()),
        None => (pdfium.load_pdf_from_file(percorso, None)?, &[][..]),
    };
    // Come scale_page_to_display_size, ma senza ruotare le pagine orizzontali:
    // l'anteprima deve mostrare la pagina com'è.
    let config = PdfRenderConfig::new()
        .set_target_width(LATO_MAX)
        .set_maximum_width(LATO_MAX)
        .set_maximum_height(LATO_MAX);
    let mut out = Vec::new();
    for (n, pagina) in doc.pages().iter().take(PAGINE_MAX).enumerate() {
        let rgba = pagina.render_with_config(&config)?.as_image()?.to_rgba8();
        let dim = [rgba.width() as usize, rgba.height() as usize];
        out.push(Pagina {
            immagine: ColorImage::from_rgba_unmultiplied(dim, &rgba),
            mm: Some((pagina.width().to_mm() as f64, pagina.height().to_mm() as f64)),
            impaginata: posizioni.get(n).is_some_and(Option::is_some),
            ruotata: posizioni.get(n).copied().flatten().is_some_and(|p| p.ruota),
        });
    }
    Ok(out)
}

/// Anteprima di un'immagine. La scala cambia solo i DPI (i pixel restano) e,
/// se c'è, l'impaginazione passa dalla stessa `componi_raster` della stampa.
fn immagine(
    percorso: &Path,
    formato: Formato,
    lav: &Lavorazione,
    imp: Option<&Impaginazione>,
) -> Option<Pagina> {
    let originale = image::open(percorso).ok()?;
    let ridotta = originale.thumbnail(LATO_MAX as u32, LATO_MAX as u32);
    let colori = |img: &image::RgbaImage| {
        ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img)
    };
    let (dpi_x, dpi_y) = file::dpi_immagine(percorso, formato);
    let (larghezza, altezza) = (
        originale.width() as f64 * 25.4 / dpi_x,
        originale.height() as f64 * 25.4 / dpi_y,
    );
    let (sx, sy) = lavorazione::fattori(&lav.scala, larghezza, altezza);
    let mm = Some((larghezza * sx, altezza * sy));
    // DPI della miniatura: quelli del file, scalati come i pixel e dalla scala.
    let dpi = (
        dpi_x * ridotta.width() as f64 / originale.width() as f64 / sx,
        dpi_y * ridotta.height() as f64 / originale.height() as f64 / sy,
    );
    let ridotta = png::su_bianco(ridotta);
    let Some(imp) = imp else {
        return Some(Pagina {
            immagine: colori(&image::DynamicImage::ImageRgb8(ridotta).to_rgba8()),
            mm,
            impaginata: false,
            ruotata: false,
        });
    };
    match impagina::componi_raster(ridotta, dpi, imp) {
        Ok(c) => {
            let mm = (c.larghezza_px as f64 * 25.4 / c.dpi.0, c.altezza_px() as f64 * 25.4 / c.dpi.1);
            let rotolo = image::DynamicImage::ImageRgb8(c.su_rotolo()).thumbnail(LATO_MAX as u32, LATO_MAX as u32);
            Some(Pagina { immagine: colori(&rotolo.to_rgba8()), mm: Some(mm), impaginata: true, ruotata: c.ruotata })
        }
        Err(ridotta) => Some(Pagina {
            immagine: colori(&image::DynamicImage::ImageRgb8(ridotta).to_rgba8()),
            mm,
            impaginata: false,
            ruotata: false,
        }),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::impagina::Allineamento;
    use crate::lavorazione::{FormatoIso, Scala};
    use lopdf::dictionary;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static CONTATORE: AtomicU64 = AtomicU64::new(0);
    fn file_temp(suffisso: &str) -> PathBuf {
        let n = CONTATORE.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("iqclock-test-{}-{n}{suffisso}", std::process::id()))
    }

    /// PDF minimo con una pagina per ogni MediaBox `(larghezza, altezza)` in pt.
    fn crea_pdf(pagine_pt: &[(i32, i32)]) -> PathBuf {
        let mut doc = lopdf::Document::with_version("1.5");
        let pagine_id = doc.new_object_id();
        let mut kids = Vec::new();
        for &(l, a) in pagine_pt {
            let pagina = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pagine_id,
                "MediaBox" => vec![0.into(), 0.into(), l.into(), a.into()],
            });
            kids.push(pagina.into());
        }
        let n = pagine_pt.len() as i64;
        doc.objects.insert(
            pagine_id,
            lopdf::Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => kids, "Count" => lopdf::Object::Integer(n),
            }),
        );
        let catalogo = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pagine_id });
        doc.trailer.set("Root", catalogo);
        let pdf = file_temp(".pdf");
        doc.save(&pdf).unwrap();
        pdf
    }

    /// Pdfium caricato davvero, oppure `None` con avviso (sviluppo locale senza
    /// `scarica-pdfium.sh`). In CI la libreria deve esserci: lì l'assenza è un
    /// errore, altrimenti i test di rendering passerebbero senza girare.
    fn pdfium_o_salta() -> Option<Pdfium> {
        match carica_pdfium() {
            Ok(p) => Some(p),
            Err(e) if std::env::var_os("CI").is_some() => panic!("pdfium non caricato in CI: {e}"),
            Err(e) => {
                eprintln!("pdfium assente ({e}): test saltato");
                None
            }
        }
    }

    #[test]
    fn rende_un_pdf_a1() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        // Una pagina A1 (1684 × 2384 pt) vuota.
        let pdf = crea_pdf(&[(1684, 2384)]);

        let pagine = pagine_pdf(&pdfium, &pdf, &Lavorazione::default(), None).unwrap();
        std::fs::remove_file(&pdf).unwrap();
        assert_eq!(pagine.len(), 1);
        let [l, a] = pagine[0].immagine.size;
        assert_eq!(a, LATO_MAX as usize);
        assert!((l as f64 / a as f64 - 1684.0 / 2384.0).abs() < 0.01);
        let (l_mm, a_mm) = pagine[0].mm.unwrap();
        assert!((l_mm - 594.0).abs() < 1.0 && (a_mm - 841.0).abs() < 1.0);
    }

    #[test]
    fn anteprima_di_un_pdf_scalato() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        // A1 (1684 × 2384 pt) adattata ad A4: 209,8 × 297 mm.
        let pdf = crea_pdf(&[(1684, 2384)]);
        let lav = Lavorazione { scala: Scala::Formato(FormatoIso::A4) };
        let pagine = pagine_pdf(&pdfium, &pdf, &lav, None).unwrap();
        std::fs::remove_file(&pdf).unwrap();
        assert_eq!(pagine.len(), 1);
        let (l_mm, a_mm) = pagine[0].mm.unwrap();
        assert!((l_mm - 210.0).abs() < 1.0 && (a_mm - 297.0).abs() < 1.0, "{l_mm} × {a_mm}");
        let [l, a] = pagine[0].immagine.size;
        assert!((l as f64 / a as f64 - 210.0 / 297.0).abs() < 0.01);
        // Non è un'impaginazione sul rotolo: la pagina resta dov'è.
        assert!(!pagine[0].impaginata && !pagine[0].ruotata);
    }

    #[test]
    fn mostra_solo_le_prime_quattro_pagine() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        // Sei pagine A4 vuote: l'anteprima ne mostra al massimo PAGINE_MAX.
        let pdf = crea_pdf(&[(595, 842); 6]);
        let pagine = pagine_pdf(&pdfium, &pdf, &Lavorazione::default(), None).unwrap();
        std::fs::remove_file(&pdf).unwrap();
        assert_eq!(pagine.len(), PAGINE_MAX);
    }

    #[test]
    fn pdf_corrotto_e_inesistente_danno_errore() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        let corrotto = file_temp(".pdf");
        std::fs::write(&corrotto, b"%PDF-1.5\nnon un pdf vero").unwrap();
        assert!(pagine_pdf(&pdfium, &corrotto, &Lavorazione::default(), None).is_err(), "PDF corrotto");
        std::fs::remove_file(&corrotto).unwrap();
        assert!(
            pagine_pdf(&pdfium, &file_temp(".pdf"), &Lavorazione::default(), None).is_err(),
            "PDF inesistente"
        );
    }

    /// Senza libpdfium il messaggio dice cosa manca e che i PDF restano senza
    /// anteprima; `carica_pdfium_in` con fallback disattivato lo rende
    /// riproducibile anche dove pdfium è installato.
    #[test]
    fn pdfium_mancante_messaggio_chiaro() {
        let inesistente = Path::new(env!("CARGO_MANIFEST_DIR")).join("pdfium-cella-inesistente");
        let Err(msg) = carica_pdfium_in(std::slice::from_ref(&inesistente), false) else {
            panic!("una cartella inesistente non può dare pdfium");
        };
        assert!(msg.contains("anteprima dei PDF non disponibile"), "{msg}");
        assert!(msg.contains(&*Pdfium::pdfium_platform_library_name().to_string_lossy()), "{msg}");
    }

    fn png_valido(l: u32, a: u32) -> PathBuf {
        let p = file_temp(".png");
        image::RgbImage::new(l, a).save(&p).unwrap();
        p
    }

    /// Senza pdfium i PDF danno zero pagine ma le immagini funzionano comunque
    /// (l'app resta utilizzabile) e gli altri formati danno zero pagine.
    #[test]
    fn senza_pdfium_pdf_vuoto_ma_immagini_funzionano() {
        let senza: Result<Pdfium, String> = Err(messaggio_pdfium_mancante());
        let pdf = crea_pdf(&[(595, 842)]);
        assert!(
            pagine_per_richiesta(&senza, &pdf, Formato::Pdf, &Lavorazione::default(), None).is_empty()
        );
        std::fs::remove_file(&pdf).unwrap();
        assert!(
            pagine_per_richiesta(
                &senza,
                &file_temp(".pdf"),
                Formato::PostScript,
                &Lavorazione::default(),
                None
            )
            .is_empty()
        );
        assert!(
            pagine_per_richiesta(
                &senza,
                &file_temp(".pdf"),
                Formato::Hpgl2,
                &Lavorazione::default(),
                None
            )
            .is_empty()
        );

        let png = png_valido(60, 40);
        let pagine = pagine_per_richiesta(&senza, &png, Formato::Png, &Lavorazione::default(), None);
        std::fs::remove_file(&png).unwrap();
        assert_eq!(pagine.len(), 1);
        // Le misure ci sono sempre, anche senza impaginazione: 60 × 40 px a
        // 300 dpi presunti.
        let (l, a) = pagine[0].mm.unwrap();
        assert!((l - 60.0 * 25.4 / 300.0).abs() < 0.01 && (a - 40.0 * 25.4 / 300.0).abs() < 0.01);
    }

    #[test]
    fn immagine_corrotta_da_zero_pagine() {
        let p = file_temp(".png");
        std::fs::write(&p, b"non un'immagine").unwrap();
        assert!(immagine(&p, Formato::Png, &Lavorazione::default(), None).is_none());
        let senza: Result<Pdfium, String> = Err(messaggio_pdfium_mancante());
        assert!(
            pagine_per_richiesta(&senza, &p, Formato::Png, &Lavorazione::default(), None).is_empty()
        );
        assert!(
            pagine_per_richiesta(&senza, &p, Formato::Jpeg, &Lavorazione::default(), None).is_empty()
        );
        assert!(
            pagine_per_richiesta(&senza, &p, Formato::Tiff, &Lavorazione::default(), None).is_empty()
        );
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn immagine_grande_viene_ridotta_mantenendo_le_proporzioni() {
        let p = png_valido(2000, 1000);
        let img = immagine(&p, Formato::Png, &Lavorazione::default(), None).expect("PNG valido");
        std::fs::remove_file(&p).unwrap();
        let [l, a] = img.immagine.size;
        assert_eq!(l, LATO_MAX as usize);
        assert_eq!(a, LATO_MAX as usize / 2);
    }

    /// Il thread anteprime consegna comunque le immagini (con o senza pdfium):
    /// l'app resta utilizzabile anche se la libreria manca.
    #[test]
    fn il_thread_consegna_le_immagini() {
        let png = png_valido(60, 40);
        let (tx, rx) = std::sync::mpsc::channel();
        let (anteprime, _) = Anteprime::avvia(move |id, pagine| {
            let _ = tx.send((id, pagine.len()));
        });
        anteprime.richiedi(7, png.clone(), Formato::Png, Lavorazione::default(), None);
        let (id, n) = rx.recv_timeout(Duration::from_secs(10)).expect("consegna anteprima");
        std::fs::remove_file(&png).unwrap();
        assert_eq!((id, n), (7, 1));
    }

    /// L'anteprima di un PDF impaginato è il file che parte: pagina larga
    /// quanto il rotolo e contenuto nero dove deve stare. Il rendering di pdfium
    /// verifica anche il verso di /Rotate.
    #[test]
    fn pdf_impaginato_al_centro_e_a_destra() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        let mm_pt = 72.0 / 25.4;
        // Rotolo da 400 mm, margine 10: un 100 × 50 mm orizzontale resta dritto,
        // un 50 × 100 verticale viene ruotato e diventa 100 × 50.
        for (l, a, ruotata) in [(100.0, 50.0, false), (50.0, 100.0, true)] {
            // Pagina da 380 mm (400 meno 10 per lato).
            for (allineamento, x_atteso) in [(Allineamento::Centro, 140.0), (Allineamento::Destra, 280.0)] {
                let imp = Impaginazione { allineamento, rotolo_mm: 400.0, margine_mm: 10.0 };
                let pdf = file_temp(".pdf");
                std::fs::write(&pdf, crate::impagina::test::pdf(l * mm_pt, a * mm_pt, (20.0, 40.0), 0)).unwrap();
                let pagine = pagine_pdf(&pdfium, &pdf, &Lavorazione::default(), Some(&imp)).unwrap();
                std::fs::remove_file(&pdf).unwrap();
                assert!(pagine[0].impaginata);
                assert_eq!(pagine[0].ruotata, ruotata);
                let (l_mm, a_mm) = pagine[0].mm.unwrap();
                assert!((l_mm - 380.0).abs() < 0.5 && (a_mm - 50.0).abs() < 0.5, "{l_mm} × {a_mm}");
                // Colonne nere della riga centrale, in mm dal bordo sinistro.
                let img = &pagine[0].immagine;
                let [lp, ap] = img.size;
                let px_mm = lp as f64 / 380.0;
                let nere: Vec<usize> = (0..lp).filter(|&x| img.pixels[ap / 2 * lp + x].r() < 128).collect();
                let (prima, ultima) = (nere[0] as f64 / px_mm, (*nere.last().unwrap() + 1) as f64 / px_mm);
                assert!((prima - x_atteso).abs() < 1.0, "{allineamento:?} {l}×{a}: inizia a {prima} mm");
                assert!((ultima - (x_atteso + 100.0)).abs() < 1.0, "{allineamento:?} {l}×{a}: finisce a {ultima} mm");
            }
        }
    }

    #[test]
    fn immagine_impaginata_larga_quanto_il_rotolo() {
        // 600 × 300 px senza DPI (300 presunti): 50,8 × 25,4 mm su rotolo da 200.
        let p = file_temp(".png");
        image::RgbImage::new(600, 300).save(&p).unwrap();
        let imp = Impaginazione { allineamento: Allineamento::Centro, rotolo_mm: 200.0, margine_mm: 5.0 };
        let pagina = immagine(&p, Formato::Png, &Lavorazione::default(), Some(&imp)).unwrap();
        std::fs::remove_file(&p).unwrap();
        let (l_mm, a_mm) = pagina.mm.unwrap();
        assert!((l_mm - 190.0).abs() < 0.5 && (a_mm - 25.4).abs() < 0.5, "{l_mm} × {a_mm}");
        let [lp, ap] = pagina.immagine.size;
        assert!(lp <= LATO_MAX as usize);
        let riga = &pagina.immagine.pixels[ap / 2 * lp..(ap / 2 + 1) * lp];
        let nere: Vec<usize> = (0..lp).filter(|&x| riga[x].r() < 128).collect();
        let centro = (nere[0] + nere.last().unwrap()) as f64 / 2.0 / lp as f64;
        assert!((centro - 0.5).abs() < 0.01, "centro a {centro}");
    }
}
