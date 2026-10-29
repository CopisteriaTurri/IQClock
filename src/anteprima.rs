//! Rendering delle anteprime (prime pagine dei PDF con pdfium, immagini con `image`)
//! su un thread dedicato: pdfium non è thread-safe, quindi una sola istanza lo usa.
//!
//! La libreria pdfium (`pdfium.dll` / `libpdfium.so`, build chromium/7881 di
//! bblanchon/pdfium-binaries) va messa accanto all'eseguibile. Se manca, i PDF
//! restano senza anteprima e il resto funziona.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use eframe::egui::ColorImage;
use pdfium_render::prelude::*;

use crate::pjl::Formato;

/// Pagine renderizzate per documento (quante ne mostra l'anteprima sul rotolo).
pub const PAGINE_MAX: usize = 4;
/// Lato lungo massimo delle immagini di anteprima, in pixel.
const LATO_MAX: i32 = 1400;

/// Una pagina renderizzata con le sue misure (le pagine di un PDF possono
/// avere formati diversi; per le immagini le misure non si conoscono).
pub struct Pagina {
    pub immagine: ColorImage,
    pub mm: Option<(f64, f64)>,
}

pub struct Anteprime {
    richieste: Sender<(u64, PathBuf, Formato)>,
}

impl Anteprime {
    /// Avvia il thread. `consegna` riceve l'id del documento e le sue pagine.
    /// Restituisce anche l'errore di caricamento di pdfium, se c'è stato.
    pub fn avvia(consegna: impl Fn(u64, Vec<Pagina>) + Send + 'static) -> (Self, Option<String>) {
        let (richieste, rx) = channel::<(u64, PathBuf, Formato)>();
        let (esito_tx, esito_rx) = channel();
        thread::spawn(move || {
            let pdfium = carica_pdfium();
            let _ = esito_tx.send(pdfium.as_ref().err().cloned());
            for (id, percorso, formato) in rx {
                consegna(id, pagine_per_richiesta(&pdfium, &percorso, formato));
            }
        });
        let errore = esito_rx.recv().unwrap_or_else(|_| Some("thread anteprime terminato".into()));
        (Self { richieste }, errore)
    }

    pub fn richiedi(&self, id: u64, percorso: PathBuf, formato: Formato) {
        let _ = self.richieste.send((id, percorso, formato));
    }
}

/// Una richiesta del thread anteprime: i PDF senza pdfium (o illeggibili)
/// danno zero pagine ma non bloccano il resto (le immagini funzionano comunque).
fn pagine_per_richiesta(pdfium: &Result<Pdfium, String>, percorso: &Path, formato: Formato) -> Vec<Pagina> {
    match formato {
        Formato::Pdf => match pdfium {
            Ok(p) => pagine_pdf(p, percorso).unwrap_or_default(),
            Err(_) => Vec::new(),
        },
        Formato::Jpeg | Formato::Tiff | Formato::Png => {
            immagine(percorso).map(|immagine| Pagina { immagine, mm: None }).into_iter().collect()
        }
        Formato::PostScript | Formato::Hpgl2 => Vec::new(),
    }
}

/// Cerca pdfium accanto all'eseguibile, poi (in debug) in `pdfium/<piattaforma>`
/// del progetto, infine tra le librerie di sistema.
fn carica_pdfium() -> Result<Pdfium, String> {
    carica_pdfium_in(&cartelle_pdfium(), true)
}

fn cartelle_pdfium() -> Vec<PathBuf> {
    let mut cartelle: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        cartelle.push(dir);
    }
    if cfg!(debug_assertions) {
        let piattaforma = if cfg!(windows) { "win-x64" } else { "linux-x64" };
        cartelle.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("pdfium").join(piattaforma));
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

fn pagine_pdf(pdfium: &Pdfium, percorso: &Path) -> Result<Vec<Pagina>, PdfiumError> {
    let doc = pdfium.load_pdf_from_file(percorso, None)?;
    // Come scale_page_to_display_size, ma senza ruotare le pagine orizzontali:
    // l'anteprima deve mostrare la pagina com'è.
    let config = PdfRenderConfig::new()
        .set_target_width(LATO_MAX)
        .set_maximum_width(LATO_MAX)
        .set_maximum_height(LATO_MAX);
    let mut out = Vec::new();
    for pagina in doc.pages().iter().take(PAGINE_MAX) {
        let rgba = pagina.render_with_config(&config)?.as_image()?.to_rgba8();
        let dim = [rgba.width() as usize, rgba.height() as usize];
        out.push(Pagina {
            immagine: ColorImage::from_rgba_unmultiplied(dim, &rgba),
            mm: Some((pagina.width().to_mm() as f64, pagina.height().to_mm() as f64)),
        });
    }
    Ok(out)
}

fn immagine(percorso: &Path) -> Option<ColorImage> {
    let img = image::open(percorso).ok()?.thumbnail(LATO_MAX as u32, LATO_MAX as u32).to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], &img))
}

#[cfg(test)]
mod test {
    use super::*;
    use lopdf::dictionary;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    static CONTATORE: AtomicU64 = AtomicU64::new(0);
    /// pdfium non è thread-safe e ammette un solo binding per processo: i test che
    /// lo usano si alternano su questo lock invece di renderizzare in parallelo.
    static PDFIUM_TEST: Mutex<()> = Mutex::new(());

    fn pdfium_seriale() -> MutexGuard<'static, ()> {
        PDFIUM_TEST.lock().unwrap_or_else(|e| e.into_inner())
    }

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
    /// `scarica-pdfium.sh`; in CI la libreria c'è e il test gira per intero).
    fn pdfium_o_salta() -> Option<Pdfium> {
        match carica_pdfium() {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("pdfium assente ({e}): test saltato");
                None
            }
        }
    }

    #[test]
    fn rende_un_pdf_a1() {
        let _guard = pdfium_seriale();
        let Some(pdfium) = pdfium_o_salta() else { return };
        // Una pagina A1 (1684 × 2384 pt) vuota.
        let pdf = crea_pdf(&[(1684, 2384)]);

        let pagine = pagine_pdf(&pdfium, &pdf).unwrap();
        std::fs::remove_file(&pdf).unwrap();
        assert_eq!(pagine.len(), 1);
        let [l, a] = pagine[0].immagine.size;
        assert_eq!(a, LATO_MAX as usize);
        assert!((l as f64 / a as f64 - 1684.0 / 2384.0).abs() < 0.01);
        let (l_mm, a_mm) = pagine[0].mm.unwrap();
        assert!((l_mm - 594.0).abs() < 1.0 && (a_mm - 841.0).abs() < 1.0);
    }

    #[test]
    fn mostra_solo_le_prime_quattro_pagine() {
        let _guard = pdfium_seriale();
        let Some(pdfium) = pdfium_o_salta() else { return };
        // Sei pagine A4 vuote: l'anteprima ne mostra al massimo PAGINE_MAX.
        let pdf = crea_pdf(&[(595, 842); 6]);
        let pagine = pagine_pdf(&pdfium, &pdf).unwrap();
        std::fs::remove_file(&pdf).unwrap();
        assert_eq!(pagine.len(), PAGINE_MAX);
    }

    #[test]
    fn pdf_corrotto_e_inesistente_danno_errore() {
        let _guard = pdfium_seriale();
        let Some(pdfium) = pdfium_o_salta() else { return };
        let corrotto = file_temp(".pdf");
        std::fs::write(&corrotto, b"%PDF-1.5\nnon un pdf vero").unwrap();
        assert!(pagine_pdf(&pdfium, &corrotto).is_err(), "PDF corrotto");
        std::fs::remove_file(&corrotto).unwrap();
        assert!(pagine_pdf(&pdfium, &file_temp(".pdf")).is_err(), "PDF inesistente");
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
        assert!(pagine_per_richiesta(&senza, &pdf, Formato::Pdf).is_empty());
        std::fs::remove_file(&pdf).unwrap();
        assert!(pagine_per_richiesta(&senza, &file_temp(".pdf"), Formato::PostScript).is_empty());
        assert!(pagine_per_richiesta(&senza, &file_temp(".pdf"), Formato::Hpgl2).is_empty());

        let png = png_valido(60, 40);
        let pagine = pagine_per_richiesta(&senza, &png, Formato::Png);
        std::fs::remove_file(&png).unwrap();
        assert_eq!(pagine.len(), 1);
        assert!(pagine[0].mm.is_none());
    }

    #[test]
    fn immagine_corrotta_da_zero_pagine() {
        let p = file_temp(".png");
        std::fs::write(&p, b"non un'immagine").unwrap();
        assert!(immagine(&p).is_none());
        let senza: Result<Pdfium, String> = Err(messaggio_pdfium_mancante());
        assert!(pagine_per_richiesta(&senza, &p, Formato::Png).is_empty());
        assert!(pagine_per_richiesta(&senza, &p, Formato::Jpeg).is_empty());
        assert!(pagine_per_richiesta(&senza, &p, Formato::Tiff).is_empty());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn immagine_grande_viene_ridotta_mantenendo_le_proporzioni() {
        let p = png_valido(2000, 1000);
        let img = immagine(&p).expect("PNG valido");
        std::fs::remove_file(&p).unwrap();
        let [l, a] = img.size;
        assert_eq!(l, LATO_MAX as usize);
        assert_eq!(a, LATO_MAX as usize / 2);
    }

    /// Il thread anteprime consegna comunque le immagini (con o senza pdfium):
    /// l'app resta utilizzabile anche se la libreria manca.
    #[test]
    fn il_thread_consegna_le_immagini() {
        let _guard = pdfium_seriale();
        let png = png_valido(60, 40);
        let (tx, rx) = std::sync::mpsc::channel();
        let (anteprime, _) = Anteprime::avvia(move |id, pagine| {
            let _ = tx.send((id, pagine.len()));
        });
        anteprime.richiedi(7, png.clone(), Formato::Png);
        let (id, n) = rx.recv_timeout(Duration::from_secs(10)).expect("consegna anteprima");
        std::fs::remove_file(&png).unwrap();
        assert_eq!((id, n), (7, 1));
    }
}
