# **Pestaña Limpieza**

**Nota:** las capturas de pantalla están tomadas con la interfaz en ruso. Rehacerlas en español es una tarea que espera a una persona voluntaria: los pull requests son bienvenidos.

![image](../images/Вкладка-Клининг/1.png)

Sirve para borrar el texto original de las páginas del cómic.
Por ahora tiene 2 herramientas: `Pintar` y `Eliminación con IA`

## **¿Ha desaparecido la limpieza?**
A partir de la 2.7 se cambió la estructura. Ahora la limpieza se guarda en la carpeta **projects/{serie}/{capítulo}/clean_layers** y no en **cleaned**. Simplemente copie las imágenes a la carpeta nueva.

## **Panel superior**
- `Vaciar la capa`: borra todo lo que se haya dibujado
- `Mostrar la capa`: alterna la visibilidad de la superposición de dibujo. Permite ver qué se ha dibujado ahora y qué había ya en la imagen original.
- `Limpieza rápida`: si se ha hecho la detección de texto; descripción completa más abajo.
- `Guardar limpieza`: guarda las imágenes en la carpeta de limpieza del proyecto

## **Herramienta Pintar**
![image](../images/Вкладка-Клининг/2.png)

Pincel rápido con cuentagotas, borrador y rectángulo. Sirve para tapar texto sobre un fondo uniforme.

- El cursor muestra el tamaño del área de dibujo y el color actual (en este caso, `rojo`)
- `Clic izq.`: dibujo normal
- `Clic der.`: cuentagotas, toma el color de debajo del centro del cursor
- `Shift+clic izq.`: borrador
- `Shift+rueda del ratón`: tamaño del pincel
- `Ctrl+clic izq.`: selección y relleno de un área rectangular, para una limpieza aún más rápida

## **Herramienta Eliminación con IA**
![image](../images/Вкладка-Клининг/3.png)
![image](../images/Вкладка-Клининг/4.png)

Selecciona un área de la tira y elimina los objetos bajo la máscara. Usa la IA del repositorio `advimman/lama`

- Con `Shift+clic izq.` seleccione un área de la tira
  - **Si la ventana no se ha abierto, es que en la selección ha entrado más de una página**
- Se abrirá una ventana nueva para dibujar la máscara
  - Dibujar la máscara con el `clic izq.`
  - Borrar la máscara con el `clic der.`
  - Cambiar el tamaño del pincel con `Shift+rueda del ratón`
- El botón `Procesar` lanzará la IA y eliminará el objeto bajo la máscara
- Se puede activar `Refine`, a veces da un resultado algo mejor
- Si algo no sale bien, se puede pulsar `Cancelar` y volver a dibujar la máscara
- Se puede volver a seleccionar con la máscara y eliminar los artefactos
- El botón `Cerrar` simplemente cierra esta ventana
- El botón `Aplicar` superpone el área modificada sobre la tira

### Otros modelos de IA

- `Lama MPE`: un modelo Lama más pequeño y algo más tonto, del repositorio zyddnys/manga-image-translator. Pero a veces funciona mejor con el estilo anime y los cómics que la Lama normal
- `AOT`: un modelo bastante pequeño, entrenado con manga. También de zyddnys/manga-image-translator

## **Herramienta Degradado**
![image](../images/Вкладка-Клининг/5.png)
![image](../images/Вкладка-Клининг/6.png)

Selecciona un área de la tira e intenta reconstruir el degradado bajo la máscara. A menudo rellena el degradado mejor que la IA

- El manejo es igual que el de la herramienta `Eliminación con IA`
- No se estropea si bajo la máscara cae un trozo de color estable
- El programa puede quedarse colgado un rato, es normal

## **Herramienta Tampón**
![image](../images/Вкладка-Клининг/8.png)
![image](../images/Вкладка-Клининг/8_1.png)

Toma el área que tiene debajo del mismo lugar de otra imagen y dibuja con ella. 

Permite tomar algo de otra versión de este mismo capítulo, por ejemplo las onomatopeyas en inglés. O eliminar marcas de agua aprovechando una versión traducida a cualquier otro idioma que no las tenga.

### **Para usar esta herramienta, hay que descargar y guardar en el descargador una versión alternativa de este capítulo.**

Tiene estos parámetros:

- Original: carpeta con imágenes. Puede haber varias. En la carpeta del proyecto esto está en la carpeta alt_vers.
- Tamaño: tamaño del pincel. También se regula con **Shift+rueda del ratón**.
- Vista previa: regula la opacidad de la vista previa dentro del círculo del pincel.
- Desplazamiento Y: desplaza arriba y abajo la imagen de la que se toma el área. Útil si allí se insertaron banners.

Manejo:

- Clic izq.: dibujar
- Clic der.: borrador
- Shift+clic izq.: borrador (selección rectangular)
- Ctrl+clic izq.: rellenar (selección rectangular)

## **Limpieza rápida**
![image](../images/Вкладка-Клининг/7.png)
![image](../images/Вкладка-Клининг/7_1.png)

![image](../images/Вкладка-Клининг/7_2.png)

### **Primero haga la detección de texto en la pestaña Traducción**

Usa la máscara del texto obtenida tras su detección en la pestaña de traducción para intentar tapar el texto sobre un fondo uniforme.
Solo pinta si el color de los bordes de la máscara es el mismo.

- `Expansión automática de la máscara`: cuánto expandir la máscara si el color resultó no ser uniforme. Ayuda a limpiar un poco más de texto. Se aplica 1 vez.


## **¿Cómo hacer la limpieza en Photoshop?**
### Limpieza completa
- Tome las imágenes de **projects/{serie}/{capítulo}/scr**
- Procéselas en Photoshop
- Guárdelas en la carpeta **projects/{serie}/{capítulo}/clean_layers** y reinicie el programa

### Procesar un área difícil
- Seleccione el área con una de las herramientas de edición de área (OpenCV/Degradado/IA)
- Sin cambiar nada, pulse **Aplicar**; esa área se trasladará a la capa transparente de limpieza
- Pulse **Guardar capas**
- Abra en Photoshop la imagen correspondiente de la carpeta **projects/{serie}/{capítulo}/clean_layers**
- Procésela, guárdela y reinicie el programa
