# Ventana **Nuevo proyecto**

**Nota:** las capturas de pantalla están tomadas con la interfaz en ruso. Rehacerlas en español es una tarea que espera a una persona voluntaria: los pull requests son bienvenidos.

![image](../images/Окно-Новый-проект/1.png)

Descarga el capítulo de distintos sitios y lo procesa previamente.

## Procesamiento por lotes
Descarga y procesamiento masivos de capítulos basados en un grafo de nodos. Todavía sin terminar y sin pulir. Funciona parcialmente. No le haga caso.


## **Importación**
El botón `Abrir carpeta` permite abrir una carpeta con imágenes de la manhwa e importarlas.

- Se puede abrir una carpeta con un capítulo ya descargado; en ese caso las imágenes deben estar nombradas en el orden correcto, por ejemplo `1.png/jpg/jpeg`
- Se puede abrir un sitio con el capítulo guardado desde el navegador principal. 
  - En ese caso el programa examinará el archivo `html` que está un nivel por encima y tiene el nombre de la carpeta, y cargará las imágenes en el mismo orden en el que estaban en la página.
  - Si no se encuentra el archivo HTML, el programa intentará cargar las imágenes o los `resource(X)` como imágenes según el orden de sus nombres.
  - Se puede definir un patrón de nombres de archivo si los archivos de imagen tienen nombres poco habituales
- El filtro de ±50 % de ancho funciona bien con los cómics de formato vertical y ayuda a quitar las imágenes de publicidad, pero **es mejor desactivarlo para el manga y otros cómics por páginas**, porque si no pueden desaparecer páginas

El botón `Abrir archivo` permite abrir una imagen suelta, un archivo comprimido o el archivo html de un sitio descargado.

El botón `Pegar desde el portapapeles` permite pegar una única imagen copiada.

`Modo de adición`: actívelo para no borrar la tira por completo al añadir una imagen olvidada.

## **Descargador rápido**
![image](../images/Окно-Новый-проект/2.png)

- El campo de entrada de arriba y el botón de descarga permiten descargar rápidamente un capítulo gratuito de comic.naver.com, **¡No de series.naver.com!**
- Ponga el cursor sobre el botón de descarga para ver los sitios admitidos.


## **Descargador avanzado**
![image](../images/Окно-Новый-проект/3_1.png)
![image](../images/Окно-Новый-проект/3_2.png)
![image](../images/Окно-Новый-проект/3_3.png)

Abre la página indicada en un navegador completo y descarga las imágenes por el método elegido.

### **Intercepción profunda**
El modo más sencillo y universal, que funciona incluso con sitios complicados. Pero **solo funciona con CloakBrowser**, y descarga de la página todo lo que parezca una imagen. **Cuando termine se abrirá una ventana, y habrá que desactivar a mano las imágenes que no pertenezcan al capítulo, por ejemplo la publicidad.**

## **Descarga de Canvas desde la página**
Su funcionalidad ya está integrada en la intercepción profunda, así que puede no tocarla. Descarga las imágenes en el caso de que sean `<canvas>` y no `<img>`.

## **Búsqueda de enlaces por patrón**
Un método más limpio, pero más engorroso, que no funciona en todas partes. **HACEN FALTA NOCIONES BÁSICAS DE HURGAR EN EL CÓDIGO DE LA PÁGINA**, hay una guía al final de esta wiki.

Busca enlaces según un patrón de prefijo:
- `*` significa cualquier combinación de caracteres
- `?` significa un carácter cualquiera
- Es un prefijo, así que lo importante es su comienzo. El final inestable se puede omitir.

Los prefijos se pueden guardar y cargar.

### Recopilación de enlaces
Ayuda si en la página no han aparecido todas las imágenes de golpe. Por ejemplo, si el sitio las carga sobre la marcha, o si es un lector por páginas.

**En ese caso, inicie la recopilación, recorra todo el capítulo y detenga la recopilación.**

### Hilos de descarga
La descarga multihilo es mucho más rápida, pero no siempre funciona. Si hay que obtener las imágenes usando la sesión del navegador y no una petición normal, la descarga es, por desgracia, de un solo hilo.


## **Unión/Corte**
![image](../images/Окно-Новый-проект/4.png)

Une todas las imágenes en una sola tira y luego las divide de forma que no se corte por encima del texto ni de un dibujo. **¡No usar para manga!**, solo para manhwa/manhua y otros cómics en forma de tira larga.

### **Parámetros de la unión**
- `Número de partes`: en cuántas partes dividir la tira. Si está vacío, se hace automáticamente.
- `Hmax`: en partes de qué altura (en píxeles) cortar la tira en la división automática.
- `Banda blanca`: una línea de cuántos píxeles comprobar en busca de color uniforme al marcar los puntos de corte. Dicho de forma sencilla: cómo de fina puede ser una franja de un solo color para que se pueda cortar por ahí.
- `Tolerancia de color uniforme`: cuánto puede diferir el color de los píxeles en un punto por donde se puede cortar. Conviene ponerla más alta si es un shojo con montones de dibujos bonitos.
- `search radius`: hasta dónde, en ambas direcciones desde el punto de corte previsto, se buscará un lugar adecuado.

### **Modos de funcionamiento**
- `Unir la tira`: simplemente une todo en una tira larga y nada más
- `Unir y colocar líneas de corte`: une y marca los puntos de corte para revisarlos a mano. Se explican más abajo.
- `Unir y cortar automáticamente`: une y corta enseguida por los puntos óptimos. Es rápido, pero es mejor el control manual.
- `Unir solo en los puntos irregulares`: no corta, solo pega la tira allí donde los cortes pasaban por un dibujo o una textura

### **Unión y corte manuales**
Después de `Unir y colocar líneas de corte`, o de añadir una línea de corte a mano, aparece esta interfaz:
![image](../images/Окно-Новый-проект/4_5.png)
  - La **flecha roja** marca la línea de corte en la barra de desplazamiento
  - La **flecha azul** marca un **corte ya existente**
  - La **línea roja** es el futuro corte; se puede mover y eliminar
  - El **botón rojo** `Cortar` de arriba aplica todos los puntos de corte y vuelve a montar la tira

- La línea de corte se puede añadir desde el menú del clic derecho
- También, desde el menú del clic derecho, se puede unir la página actual con la siguiente y con la anterior

### **Otras acciones con la página**
![image](../images/Окно-Новый-проект/4_6.png)

Este es el menú de acciones en la esquina de cada página.
- Las flechas arriba y abajo intercambian la página actual con la siguiente o la anterior
- La cruz la elimina
- La página se puede recortar a mano


## **Cortar como capítulo**
![image](../images/Окно-Новый-проект/4_1.png)

Toma como base el capítulo elegido y corta las imágenes exactamente igual. Hace falta para descargar versiones alternativas para la herramienta Tampón.

Si hay diferencia en la altura total de ambos capítulos, se abrirá esta ventana:

![image](../images/Окно-Новый-проект/4_2.png)
![image](../images/Окно-Новый-проект/4_3.png)

Aquí hay que asegurarse de que las imágenes coinciden. La imagen del capítulo descargado será semitransparente. Hay que ajustar la altura para que quede como en la primera imagen, y no como en la segunda.

### **Después hay que guardarlo como versión alternativa del capítulo elegido, indicando un nombre.**


## **Procesamiento de imágenes (Waifu2x/Reline)**
![image](../images/Окно-Новый-проект/5.png)

## Waifu2x

IA obsoleta, pero todavía funcional, para reducir el ruido y escalar. Más sencilla y rápida que Reline

## Reline

IA moderna para reducir el ruido y escalar. Tiene muchos modelos distintos, sobre todo para manga. 


## **Guardado**
![image](../images/Окно-Новый-проект/6.png)

Guarda la serie procesada en la estructura del proyecto o simplemente en la carpeta elegida (guardado independiente).

Si simplemente está guardando el primer capítulo, elija "Guardar como base del proyecto", introduzca el nombre y pulse "Guardar y abrir".

- La serie es a la vez un campo de texto y una lista desplegable. Puede introducir el suyo propio.


# Hackear el sitio y crear el prefijo
Con el ejemplo de mto.to

## 1. Abrimos el capítulo en un navegador normal y pulsamos F12
![image](../images/Окно-Новый-проект/7.png)

## 2. Pasamos el cursor sobre las distintas etiquetas HTML y el propio navegador muestra de qué se encargan. Si queda resaltada la parte del sitio con la imagen del capítulo, vamos abriendo la etiqueta hasta llegar a la imagen en sí.
![image](../images/Окно-Новый-проект/8.png)

## 3. Abrimos la etiqueta de una imagen concreta y miramos qué enlace hay ahí.
![image](../images/Окно-Новый-проект/9.png)
### Por ejemplo, aquí tenemos el enlace `https://n27.mbeaj.org/media/mbch/a97/6921b1dc4b5d85970424179a/128472992_800_14755_1072554.webp` Lo abrimos en una pestaña nueva y comprobamos que es una imagen.

### Después, abrimos unas cuantas etiquetas de imágenes más y recopilamos los enlaces. Por ejemplo, estos:
- `https://n27.mbeaj.org/media/mbch/a97/6921b1dc4b5d85970424179a/128472992_800_14755_1072554.webp`
- `https://n25.mbuul.org/media/mbch/a97/6921b1dc4b5d85970424179a/128472994_800_12860_1448870.webp`
- `https://n21.mbrtz.org/media/mbch/a97/6921b1dc4b5d85970424179a/128473001_800_15000_1578696.webp`
- `https://n06.mbwww.org/media/mbch/a97/6921b1dc4b5d85970424179a/128473003_800_15000_1167770.webp`

## 4. Miramos los enlaces con atención y buscamos lo que tienen en común. Por ejemplo, esto:
- Por ejemplo, el subdominio siempre empieza por n
- En los nombres de los sitios siempre está mb
- La primera sección siempre es /media
- El resto, por ejemplo `mbch/a97/6921b1dc4b5d85970424179a`, puede cambiar de una serie a otra

## 5. Recordamos cómo funciona mi patrón simplificado
- `*` significa cualquier combinación de caracteres
- `?` significa un carácter suelto cualquiera

## 6. Componemos el patrón de prefijo
- Tomamos el comienzo del enlace, en este caso `https://n06.mbwww.org/media/`
- Sustituimos todo lo que cambia por comodines; por ejemplo, en lugar de `n06` pondremos `n*` o `n??`
- Añadimos * al final
- Queda algo así: `https://n*.mb*.org/media/*`

## 7. ¡Enhorabuena! `https://n*.mb*.org/media/*` se puede pegar como prefijo en el descargador avanzado
