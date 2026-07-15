# Pestaña **Traducción**

**Nota:** las capturas de pantalla están tomadas con la interfaz en ruso. Rehacerlas en español es una tarea que espera a una persona voluntaria: los pull requests son bienvenidos.

### **Las instrucciones de traducción están al final**
![image](../images/Вкладка-Перевод/1.png)
Aquí se pueden crear globos de texto, reconocer texto e insertar la traducción.

## **Globo de texto**
![image](../images/Вкладка-Перевод/2.png)

Sirve para la traducción inicial. Después permite insertar el texto rápidamente durante el tipeo.

- Se crea manualmente con la tecla T y sale hacia la izquierda o la derecha desde el punto de la tira donde estaba el cursor en el momento de la creación
- Se puede crear mediante OCR; en ese caso contendrá el texto reconocido.
- Se puede eliminar con la tecla Del
- Se puede copiar y pegar con Ctrl+C y Ctrl+V, y duplicar con Ctrl+D
- Se puede arrastrar
- **Línea superior**: el texto original
- **Línea inferior**: la traducción. A diferencia del resto de elementos, que solo están en la pestaña de traducción, esta línea estará en todas las pestañas.
- El número, en este caso `42`, es el número de la línea de diálogo para los casos en que el orden de las líneas no es de arriba abajo. Por ejemplo, en el manga. Se usará en la composición de la traducción.
- La línea que sigue al número, en este caso `Presentador`, es el nombre del personaje que habla, u otra descripción de la línea, por ejemplo `pensamientos del protagonista` o `rótulo`. La línea tiene autocompletado y sugiere los nombres de los personajes creados en la pestaña correspondiente, u otras plantillas.
- Tiene estos botones:
  - `Traducir`: traduce la línea superior e inserta el resultado en la línea inferior. Se usa el servicio elegido en el panel de traducción automática.
  - `Eliminar:` elimina el globo de texto

## **Globo con imagen**
![image](../images/Вкладка-Перевод/2_1.png)

### **Sirven sobre todo para la traducción por IA mediante API**
- Se crean con `Q` o seleccionando con `Shift+Q`
- Pueden contener un fragmento de la página dentro de la selección roja, o una imagen externa
- La `Descripción` se rellena a mano y le explica a la IA qué es esto
- El `Original` y la `Traducción` los rellena la IA
- Pueden tener varias áreas de texto a la vez dentro del marco rojo.
  - Fuera de la pestaña de traducción serán globos independientes


## **Inserción rápida del nombre del personaje**
![alt text](<../images/2-Вкладка Перевод/image-1.png>)
Arriba de la tira se muestran los 6 últimos nombres utilizados.
Para insertar rápidamente uno de ellos, mantenga pulsado el dígito correspondiente del 2 al 6 junto con la tecla rápida de creación de globo o de selección. Por ejemplo `4 + T` o `Shift + 2 + clic izq.` 

## **Reconocimiento de texto**
![image](../images/Вкладка-Перевод/6.png)

- `Shift+clic izq.` selecciona en la tira de la serie el área donde se reconocerá el texto

### EasyOCR
Motor más sencillo y universal, admite multitud de idiomas.

### PaddleOCR
OCR avanzado de ingenieros chinos, bueno para el chino, el japonés, el inglés y el coreano. Pero puede que no arranque en todos los equipos.

### MangaOCR
OCR solo de japonés, entrenado específicamente con manga. A menudo ya tiene en cuenta la lectura de derecha a izquierda en columnas.

### Surya
El motor de reconocimiento de texto más grande, no requiere elegir el idioma. En algunos casos puede ser más preciso, pero es el más lento de todos.

### AI API
Pida a ChatGPT o DeepSeek que reconozca el texto difícil. El método más caro y más preciso.

### PaddleOCR-VL
Algo intermedio entre Surya y PaddleOCR

### Ajustes
- `Mantener los saltos de línea`: se entiende por el nombre
- `Copiar al portapapeles`: si conviene copiar al portapapeles el texto reconocido
- `Columnas de derecha a izquierda`: útil al trabajar con manga, donde el texto japonés a menudo va en columnas que se leen de derecha a izquierda. Si se activa, el orden de las líneas reconocidas se invertirá.
- `Crear un globo`: si conviene crear un globo con el texto reconocido en el centro del área seleccionada
- `Reemplazar caracteres`: configure a mano qué se sustituye por qué. Por defecto sustituye los puntos centrados por puntos normales y los puntos suspensivos por tres puntos separados


## **Composición de la traducción**
![image](../images/Вкладка-Перевод/3.png)![image](../images/Вкладка-Перевод/3_1.png)

Facilita preparar las líneas de diálogo para la IA.


**Ajustes del panel de composición**

- `Ordenar`:
  - `Por altura`: cuanto más abajo esté el globo de texto en la tira, más tarde se insertará. El número de línea no importa. Normalmente para cómics de formato vertical.
  - `Por número de línea`: ignora la altura y se fija en el número de la línea.
- `Copiar`: copia la composición al portapapeles
- `Actualizar`: actualiza la composición, aunque normalmente no hace falta, ya que ocurre al abrir el panel.
- `Reemplazar \n`: por qué sustituir el salto de línea en los globos. Normalmente es un espacio, pero puede que alguien necesite delimitar las líneas de forma explícita, por ejemplo si el OCR da un orden incorrecto.
- `Envolver las líneas`: creo que se entiende por sí solo
- `Prefijo de línea`: qué insertar antes de cada línea
- `Límite de caracteres`: hasta cuántos caracteres hacer la composición. Las líneas del último personaje se insertarán completas, aunque se supere el límite.
- `Usar nombres de personajes`: si está desactivado, simplemente reunirá las líneas, envolviéndolas solo en comillas invertidas.
- `Combinar líneas del mismo personaje`: si está activado, inserta entre las líneas de un mismo personaje el parámetro siguiente
- `Entre líneas del mismo personaje`: por defecto, un salto de línea
- `Entre líneas`: qué insertar entre las líneas si los personajes son distintos. Por defecto, dos saltos de línea.

### **MiniJinja**

Permite hacer cualquier composición de líneas. Dele a la IA los parámetros disponibles del primer campo de texto, pídale que escriba la plantilla necesaria e insértela en el segundo campo.


## **Detector de texto masivo**
![image](../images/Вкладка-Перевод/7.png) ![image](../images/Вкладка-Перевод/7_1.png)

Casi como el de BallonsTranslator. Encuentra bloques de texto y los marca en azul, las líneas en verde y la máscara para la limpieza en rojo.
Tiene estos parámetros:

- `Algoritmo`: el clásico da falsos positivos más a menudo y no genera máscara. La IA, en cambio, en algunos equipos puede tardar más, pero a menudo encuentra el texto mejor y genera una máscara que después se puede usar para una limpieza rápida.
- `Mostrar los bloques detectados`: ocultar o mostrar el contorno verde de las líneas y los bloques azules
- `Mostrar la máscara`: ocultar o mostrar la máscara roja del texto
- `Expansión de bloque`: cuánto expandir cada línea verde en cada dirección. Se recomienda 5-10
- `Distancia de combinación`: a qué distancia se combinarán las líneas en bloques. Se recomienda 5
- `Reconocer`: usar el motor de reconocimiento cargado para reconocer el texto en las áreas marcadas con el marco azul.


## **Traducción automática**
![image](../images/Вкладка-Перевод/8.png)

### **Se recomienda encarecidamente NO usarla para una traducción pública.**
- Se puede usar para leer rápidamente uno mismo
- Para una traducción pública, mejor use IA como ChatGPT, Gemini, DeepSeek y similares, además del panel de composición.

## **AI API**
![image](../images/Вкладка-Перевод/8_1.png)

- Envía automáticamente el contexto de la serie y las líneas a la IA elegida
- Permite traducir solo imágenes
- La calidad ya es aceptable para una traducción pública
  - Pero aun así la calidad es mejor si se envían a mano las líneas compuestas a Gemini y luego se insertan; así resulta más fácil editar en paralelo

Por ahora solo admite Google y Yandex; DeepL todavía no funciona.

## **Panel de globos**
![image](../images/Вкладка-Перевод/5.png)

Permite buscar y editar la traducción rápidamente


## **Cómo traducir**
- Inserta en la IA la instrucción de la pestaña `Notas de traducción`
- Reconoce el texto original. Lo que no se reconozca, por ejemplo fuentes muy retorcidas, es mejor dejarlo para después, o darle directamente una captura a la IA.
- Indicas quién habla dónde
- Insertas en la IA las líneas compuestas
- Insertas el texto traducido por la IA en el globo de texto correspondiente mediante `clic der.` -> `Pegar en la traducción`
- Así traduces el texto principal
- Después, por separado, haces capturas del texto retorcido/onomatopeyas y las traduces con la IA, creando globos nuevos con T

En general, puedes traducir como quieras, con tus conocimientos del idioma o con un traductor normal, pero si no sabes el idioma, mejor usa la IA.
