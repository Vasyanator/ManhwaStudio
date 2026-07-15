# ManhwaStudio

[English](../README.md) · [Русский](README.ru.md) · **Español** · [Français](README.fr.md) · [Português](README.pt.md)

Un mini-estudio sencillo para traducir manhwa por tu cuenta. Va bien con títulos poco populares que no requieren un limpiado complicado; los títulos complejos es mejor dejarlos a equipos que trabajen con Photoshop.

> 📖 **Las funciones se describen con mucho más detalle en la [Wiki](https://github.com/Vasyanator/ManhwaStudio/tree/master/wiki/es).** La versión más actualizada también viene dentro del propio programa.

**Para sugerencias, preguntas, soporte y comunidad, te invito al servidor de Discord o a Telegram**:
- https://discord.gg/invite/mZjZszwDbH
- https://t.me/SelfTranslators

> **Nota sobre las capturas.** Todas las capturas de pantalla están tomadas con la interfaz en ruso. Reemplazarlas por capturas en español es una tarea que espera a una persona voluntaria: los pull requests son bienvenidos.

## Idea principal: burbujas de texto a los lados de la tira continua de manhwa. Las burbujas indican en qué lugar va el texto traducido.

# Menú principal
<img width="2559" height="1347" alt="image" src="https://github.com/user-attachments/assets/36b39f20-a2f4-4c9b-8ec6-a882f2ec6637" />


# Ventana de nuevo proyecto con el descargador
<img width="2559" height="1329" alt="image" src="https://github.com/user-attachments/assets/62af87db-9995-4209-bb4d-31d75128851c" />

Aquí tienes las opciones de descarga y de procesamiento inicial. Puedes simplemente abrir una carpeta, indicar un capítulo de uno de los sitios compatibles, usar el navegador con plantillas de enlaces, o abrir una copia offline de la página.
Después puedes coser y cortar la tira (para cómics verticales) y quitar el ruido con Waifu2x.

# Pestaña de traducción
<img width="2559" height="1345" alt="image" src="https://github.com/user-attachments/assets/d9fd8b7c-1eb0-4813-8dd5-b0e1108fa04e" />

Aquí se hace la traducción y la edición. El texto se puede reconocer en muchos idiomas mediante EasyOCR, PaddleOCR o MangaOCR. A cada línea se le puede asignar un rol, que se usa al componer el texto antes de enviarlo a la IA. También puedes simplemente detectar el texto y pasarlo todo por traducción automática si solo quieres leer.

# Pestaña de limpieza
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/ab996bdf-3d23-483b-b60a-4c7585355fbd" />

Aquí se cubre el texto original. Comparado con Photoshop, las funciones son bastante modestas, pero cubrir sobre un fondo uniforme resulta muy cómodo. Los modelos de IA para eliminar objetos bajo una máscara, o la herramienta de restauración de degradados, dan una calidad bastante buena. Además, los fragmentos especialmente difíciles se pueden tratar aparte en Photoshop.

# Pestaña de texto
<img width="2559" height="1344" alt="image" src="https://github.com/user-attachments/assets/a7dcf05c-3dbb-4ab1-9864-a66047d6e218" />

Aquí se coloca el texto traducido. Con Shift+clic izquierdo puedes seleccionar el área destinada a él y, si un bloque de texto lateral apunta ahí, el texto se inserta automáticamente. También puedes escribirlo a mano.
El panel ofrece distintos efectos, desde sombra y contorno hasta degradados. Las propias imágenes de texto se pueden recortar con una máscara de recorte o transformar en perspectiva. Componer el texto es bastante más rápido que en Photoshop.

# Pestaña de personajes
<img width="2559" height="1343" alt="image" src="https://github.com/user-attachments/assets/4f1ca23e-330c-4722-912d-f2c6a87d0e87" />

Aquí puedes ver y editar los personajes del título. Sus nombres y descripciones entran en las instrucciones para la traducción con IA, para mejorar su comprensión de la historia y unificar los nombres.

# Pestaña de términos
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/706f894e-b818-44a3-b961-e63f7f663770" />

Igual que los personajes, pero sin imágenes.

# Pestaña de notas de traducción
<img width="2559" height="1341" alt="image" src="https://github.com/user-attachments/assets/43297a08-851b-4f95-bc51-7cf69b9801a1" />

Aquí se escribe la instrucción principal para la IA, y en ella se insertan automáticamente los personajes y los términos.
