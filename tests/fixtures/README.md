# Fixture JPEG

Immagini sintetiche 16 × 16, senza dati esterni:

- `rgb-senza-marker.jpg`: gradiente RGB generato con Pillow 12.3 (`keep_rgb=True`), poi rimosso il segmento Adobe APP14. Il SOF contiene gli identificatori ASCII `R`, `G`, `B`; non contiene JFIF o EXIF. Serve a verificare che la scala non aggiunga un JFIF che cambierebbe l’interpretazione dei colori.
- `progressivo.jpg`: gradiente rosso/blu generato con ImageMagick (`magick -size 16x16 gradient:red-blue -interlace Plane progressivo.jpg`). Contiene più scansioni SOS, per verificare il controllo strutturale fino a EOI.

I test leggono direttamente i file; Pillow e ImageMagick non sono necessari per eseguirli.
