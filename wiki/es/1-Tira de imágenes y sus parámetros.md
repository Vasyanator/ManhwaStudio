# **Tira de imágenes:**

**Nota:** las capturas de pantalla están tomadas con la interfaz en ruso. Rehacerlas en español es una tarea que espera a una persona voluntaria: los pull requests son bienvenidos.

![alt text](<../images/1-Лента картинок и её параметры/image.png>)
![alt text](<../images/1-Лента картинок и её параметры/image-1.png>)

Aquí se muestran todas las páginas a la vez. Para el manga y los cómics por páginas hay un espacio entre las páginas; para los webtoons no lo hay. Aun así, es mejor no cortar un globo en dos páginas: para eso están la unión y el corte en la ventana de nuevo proyecto.

# **Control de la tira**
La tira se maneja como un gran lienzo: se puede desplazar, mover, acercar y crear globos directamente sobre la página.

### Desplazamiento y movimiento
La rueda del ratón desplaza la tira hacia arriba y hacia abajo. Si la tira es más ancha que la ventana por el zoom o por los globos laterales, abajo aparece una barra de desplazamiento horizontal.

Para mover la tira con la mano, mantenga pulsado `Espacio` y arrastre con el ratón. Es cómodo con mucho zoom, cuando hay que desplazar rápidamente el lienzo hacia un lado.

Arriba a la izquierda hay un pequeño panel flotante. En él se ve la página actual, la escala, el interruptor de visualización de globos y la opacidad de los globos. El panel se puede contraer y arrastrar.

### Escala
La escala cambia alrededor del punto bajo el cursor, así que al acercarse la tira no se va a otro sitio.

- `Ctrl` + rueda del ratón: acercar o alejar.
- `Z` + rueda del ratón: lo mismo, pero sin `Ctrl`.
- `Ctrl` + `+` / `Ctrl` + `-`: acercar o alejar.
- `Ctrl` + `0`: restablecer la escala.
- `Z` + `+` / `Z` + `-` / `Z` + `0`: los mismos comandos de escala mediante la tecla `Z`.
- `Ctrl` o `Z` + mantener pulsado el botón izquierdo del ratón y mover a izquierda/derecha: zoom suave arrastrando.

En macOS, en lugar de `Ctrl` se suele usar `Cmd`.

### Creación y selección de globos
En la pestaña `Traducción` se puede crear un globo nuevo con `T` en la posición del cursor. La misma acción está en el menú contextual de la página con el botón derecho del ratón.

El clic izquierdo en un espacio vacío deselecciona el globo. El clic izquierdo sobre un globo lo selecciona. `Delete` elimina el globo seleccionado.

El botón derecho del ratón sobre la página abre el menú de creación y pegado de globos. El botón derecho sobre un globo abre el menú del propio globo: allí hay acciones de copiar, pegar, duplicar, cambiar el tipo y opciones adicionales si la corrección ortográfica está activada.

### Movimiento de los globos
Los globos de tipo `Encima` se pueden mover directamente sobre la página y cambiar de tamaño con los tiradores de los bordes.

Los globos de tipo `Lateral` están en la columna lateral, pero están vinculados a un punto de la página. Se pueden arrastrar y también se puede mover el área de anclaje sobre la propia imagen. Una línea muestra a qué lugar corresponde el globo lateral.

### Deshacer y rehacer
Para las acciones con globos funcionan:

- `Ctrl` + `Z`: deshacer.
- `Ctrl` + `Shift` + `Z`: rehacer.
- `Ctrl` + `D`: duplicar el globo seleccionado.

Todas estas teclas rápidas se pueden reasignar en los ajustes de teclas rápidas.

# **Ajustes de la tira**
La tira tiene muchos ajustes, que se encuentran en la pestaña correspondiente > `Tira y globos`.
![](<../images/1-Лента картинок и её параметры/image-2.png>)

### Preajuste de configuración
Permite establecer rápidamente los ajustes estándar para webtoons o para cómics por páginas.

- **Por páginas**: las páginas se separan con un margen, los globos laterales de la pestaña de traducción se reducen más.
- **Webtoon**: las páginas van pegadas, los globos laterales no se reducen.
- **Personalizado**: se activa si los parámetros difieren de los preajustes estándar.

Esto no cambia las imágenes en sí, solo el comportamiento de la tira y de los globos.

### Tipo de globo predeterminado en la pestaña de traducción/limpieza/texto
Determina si los globos de tipo estándar se mostrarán como `Encima` o como `Lateral`.

En la pestaña de traducción esto afecta a los globos nuevos y a los globos normales sin tipo propio. En las pestañas de limpieza y texto afecta a cómo se muestran en modo de visualización los globos estándar ya existentes.

Si elige `Lateral`, el globo estará en la columna lateral y se unirá con una línea a un punto de la página. Si elige `Encima`, el globo estará directamente sobre la página.

### Insertar automáticamente el último personaje
Si está activado, al crear un globo nuevo se coloca en él inmediatamente el último personaje seleccionado. Es cómodo cuando van seguidas varias líneas del mismo personaje.

### Corregir la ortografía del original / de la traducción
Activa el resaltado ortográfico en los campos correspondientes del globo. Para la traducción suele ser útil mantenerlo activado; para el original, según el caso: el OCR a menudo da nombres, jerga y fragmentos de otro idioma que el diccionario considerará errores de todos modos.

### Palabras personalizadas para el corrector ortográfico
Aquí se pueden añadir palabras que no se deben resaltar como errores.

- **Exclusiones compartidas**: funcionan para todos los proyectos.
- **Exclusiones del proyecto**: se guardan solo para el capítulo/proyecto actual.

Escriba una palabra por línea. Es cómodo para nombres, términos, nombres de técnicas, ciudades y palabras que el diccionario no conoce.

### Estirar los globos laterales
Se encarga del ancho de los globos que están al lado de la página.

Si está activado, el ancho del globo lateral se adapta al espacio libre junto a la página, pero sin salirse del ancho mínimo y máximo. Es decir, el globo intenta no salirse de la pantalla si hay espacio para él.

Si está desactivado, los globos laterales siempre toman el ancho mínimo.

### Reducir los globos laterales en la pestaña de traducción
Este ajuste sirve para que una tira con muchos globos no se convierta en una enorme sábana de interfaz.

- **Ninguno**: el globo siempre está desplegado por completo: original, traducción, botones, número, personaje.
- **Moderado**: mientras el globo no esté seleccionado, solo se ven las líneas de original y traducción. Al enfocarlo se despliega por completo.
- **Fuerte**: mientras el globo no esté seleccionado, solo se ve la línea de traducción. Si la traducción está vacía, se muestra el original. Al enfocarlo se despliega por completo.

Para webtoons suele ser más cómodo **Ninguno**, porque las páginas forman una tira continua. Para el manga por páginas suele ser más cómodo **Fuerte**, porque las columnas laterales ocupan menos espacio.

### Lado de los globos laterales
Determina dónde mostrar los globos de tipo `Lateral`.

- **Auto**: el globo aparece a la izquierda o a la derecha según su posición en la página.
- **Izquierda**: todos los globos laterales van a la izquierda.
- **Derecha**: todos los globos laterales van a la derecha.

Con el modo **Auto** se puede mover el ancla del globo en la página, y el lado se corresponderá con su posición. Los modos forzados son cómodos si quiere mantener toda la traducción en una sola columna.

### Expansión de los globos de tipo "Encima"
Los globos de tipo `Encima` están directamente sobre la página, dentro de su rectángulo de texto. El ajuste decide qué hacer con la interfaz adicional cuando ese globo está seleccionado.

- **Alrededor**: el globo permanece sobre la página, el original se muestra arriba, el personaje y los botones abajo.
- **Lateral**: el globo seleccionado se despliega temporalmente como lateral. Es cómodo cuando no quiere tapar el dibujo con botones y campos.

### Tamaño de los globos laterales (%)
Escala la interfaz lateral: texto, botones, márgenes y la propia columna. 100 % es el tamaño normal. Menos de 100 % hace los globos laterales más compactos; más de 100 %, más grandes.

### Anchura mín. y máx. de los globos laterales
Son los límites de ancho de la columna lateral.

- **Anchura mín.**: el globo lateral no será más estrecho que esto.
- **Anchura máx.**: el globo lateral no se estirará más que esto.

Si el ancho máximo queda por accidente por debajo del mínimo, el programa lo iguala al mínimo.

### Separar páginas
Si está activado, entre las imágenes aparece un espacio separado. Es el modo normal para el manga por páginas y los cómics corrientes.

Si está desactivado, las páginas van pegadas unas a otras. Es el modo webtoon, donde todo el capítulo se lee como una única tira vertical larga.

### Espaciado entre páginas
Solo funciona cuando **Separar páginas** está activado. Cuanto mayor sea el valor, mayor será la distancia entre páginas contiguas.

Para los webtoons este parámetro no hace falta, porque las páginas no se separan.

### Margen superior/inferior
Añade espacio vacío al principio y al final de la tira. Esto no cambia las imágenes en sí, solo da un margen cómodo de desplazamiento para que la primera y la última página no queden pegadas al borde de la ventana.

### Sincronización automática entre pestañas
Sincroniza la posición de la tira entre las pestañas `Traducción`, `Limpieza` y `Texto`. Si está activado, puede pasar a otra pestaña y quedarse aproximadamente en el mismo punto del capítulo.

Si lo desactiva, cada pestaña vive con su propio desplazamiento.

### Almacenar páginas en caché
Si está activado, el programa mantiene de antemano las páginas decodificadas en memoria para operaciones rápidas. Esto acelera la limpieza, la exportación y otras acciones que necesitan los píxeles originales.

Si hay poca memoria o el capítulo es muy grande, se puede desactivar. Entonces el programa mantendrá menos cosas en memoria, pero algunas acciones pueden tardar más en abrirse.

### Estado de los globos
Los estados dibujan un borde de color alrededor de los globos según unas reglas. Esto ayuda a ver rápidamente qué líneas todavía no están listas.

Las reglas se aplican de arriba abajo: la primera regla que coincide elige el estilo del borde. Una regla tiene una condición y un borde.

Las condiciones se pueden componer a partir de bloques:

- **Traducción rellenada**
- **Original rellenado**
- **Personaje rellenado**
- **Y**: deben coincidir todas las condiciones anidadas
- **O**: basta con una condición anidada
- **NO**: invierte la condición anidada

Para el borde se puede elegir el tipo: sólido, discontinuo, punteado u ondulado, así como el color.

El preajuste estándar muestra un borde rojo si la traducción no está rellenada, y un borde verde si están rellenadas la traducción y el personaje.
