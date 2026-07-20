# ManhwaStudio

[English](../README.md) · [Русский](README.ru.md) · **Español** · [Français](README.fr.md) · [Português](README.pt.md)

Un programa especializado para traducir cómics, incluidos manga y webtoons. Una herramienta todo en uno para descargar, preprocesar, traducir, eliminar el texto original y hacer una composición tipográfica avanzada. A diferencia de sus alternativas, el programa está orientado al trabajo manual y no a la automatización, y en muchos aspectos es más cómodo e intuitivo que Photoshop.

> 📖 **Las funciones se describen con mucho más detalle en la [Wiki](https://github.com/Vasyanator/ManhwaStudio/tree/master/wiki/es).** La versión más actualizada también viene dentro del propio programa.

**Para sugerencias, preguntas, soporte y comunidad, te invito al servidor de Discord o a Telegram**:
- https://discord.gg/invite/mZjZszwDbH
- https://t.me/SelfTranslators

> **Para instalar, ve a [Releases](https://github.com/Vasyanator/ManhwaStudio/releases/latest), descarga y ejecuta el archivo ejecutable para tu sistema.** Se admiten Windows, Linux y macOS.

> **Nota sobre las capturas.** Todas las capturas de pantalla están tomadas con la interfaz en ruso. Reemplazarlas por capturas en español es una tarea que espera a una persona voluntaria: los pull requests son bienvenidos.

## Idea principal: burbujas de texto a los lados de la tira continua de páginas. Todo el capítulo se procesa de una vez. Las burbujas indican en qué lugar va el texto traducido.

# Menú principal
<img width="2559" height="1347" alt="image" src="https://github.com/user-attachments/assets/36b39f20-a2f4-4c9b-8ec6-a882f2ec6637" />


# Ventana de nuevo proyecto con el descargador
<img width="2559" height="1329" alt="image" src="https://github.com/user-attachments/assets/62af87db-9995-4209-bb4d-31d75128851c" />

- Simplemente abrir una carpeta o un archivo comprimido con el capítulo
- Descarga rápida desde los sitios compatibles
- Descarga desde la mayoría de los sitios escribiendo el prefijo correcto de los enlaces a las imágenes
- Descarga automática de todas las imágenes con selección manual de las necesarias
- Extracción de imágenes de la mayoría de los sitios guardando la página y abriendo el HTML
- Recorte cuando se trata de capturas de pantalla
- Costura/corte para webtoons. Un algoritmo inteligente que no corta donde hay dibujo
- Procesamiento con Reline o Waifu2x para escalar y eliminar ruido

# Ventana de importación desde PSD
<img width="2560" height="1356" alt="Image" src="https://github.com/user-attachments/assets/47602f13-1320-4d9e-ba71-b923c6d8b78f" />

- Abrir una carpeta o un archivo comprimido con archivos PSD tras la limpieza
- Detección automática del orden de las páginas y separación del original de la capa limpiada
- Importación de la limpieza al capítulo

# Pestaña de traducción
<img width="2559" height="1345" alt="image" src="https://github.com/user-attachments/assets/d9fd8b7c-1eb0-4813-8dd5-b0e1108fa04e" />

- Reconocimiento de texto mediante EasyOCR, MangaOCR, PaddleOCR, PaddleOCR-VL, SuryaOCR, API de IA
- Creación y edición de burbujas de traducción con indicación de personajes
- Creación de burbujas con imágenes para traducir mediante IA, por ejemplo para onomatopeyas
- Detección automática de texto y traducción automática, si solo quieres leer rápido
- Traducción a través de las API de distintos servicios de IA
- Composición de las líneas para poder exportarlas a docx o enviarlas a una IA para una traducción de mayor calidad. Para el envío a la IA hace falta indicar los personajes

# Pestaña de limpieza
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/ab996bdf-3d23-483b-b60a-4c7585355fbd" />

- Pincel rápido con cuentagotas y posibilidad de pintar con rectángulo
- Cuadrícula de píxeles con mucho aumento
- Eliminación por IA del texto sobre fondos complejos con distintos modelos, desde Lama hasta Flux
- Un algoritmo de relleno de degradado que funciona de maravilla
- Síntesis de texturas
- Limpieza rápida del texto detectado sobre fondos uniformes

# Pestaña de texto
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/a7dcf05c-3dbb-4ab1-9864-a66047d6e218" />
<img width="2556" height="1341" alt="Image" src="https://github.com/user-attachments/assets/65951a3f-386d-4c49-96d2-581cfee0acd4" />

La parte más avanzada del programa.

- Selección de un área con Shift+clic izquierdo e inserción rápida del texto de la burbuja que cae dentro de la selección
  - No hace falta que exista una burbuja. Se puede simplemente pegar una línea copiada de un documento con la traducción
- Muchos parámetros de texto, con cambio individual de algunos parámetros solo para una parte del texto (mediante selección)
- Mover, rotar, escalar y deformar la imagen del texto, línea de disposición del texto
- Máscara de recorte del texto y su relleno, que permite recortar la capa de texto en un par de clics cuando debe quedar debajo de algo
- Aplicación de parámetros al texto mientras aún está en forma vectorial. Por ejemplo rotación, estiramiento de un carácter, negrita/cursiva forzadas
- Distintos ajustes y efectos de texto, incluidos contorno, desenfoque, resplandor, degradado, reflejo y temblor

# Editor tipo PS
<img width="2560" height="1343" alt="Image" src="https://github.com/user-attachments/assets/b5b5e83c-4ccb-4b1d-bb20-97a0e55218a8" />

Todavía en desarrollo, pero ya permite trabajar con capas:
- Cortar
- Mover
- Dividir en partes
- Dibujar

# Pestaña de personajes
<img width="2559" height="1343" alt="image" src="https://github.com/user-attachments/assets/4f1ca23e-330c-4722-912d-f2c6a87d0e87" />

Aquí puedes ver y editar los personajes del título. Sus nombres y descripciones entran en las instrucciones para la traducción con IA, para mejorar su comprensión de la historia y unificar los nombres.

# Pestaña de términos
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/706f894e-b818-44a3-b961-e63f7f663770" />

Igual que los personajes, pero sin imágenes.

# Pestaña de notas de traducción
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/43297a08-851b-4f95-bc51-7cf69b9801a1" />

Aquí se escribe la instrucción principal para la IA, y en ella se insertan automáticamente los personajes y los términos.
