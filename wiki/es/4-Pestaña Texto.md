# **Pestaña Texto**

**Nota:** las capturas de pantalla están tomadas con la interfaz en ruso. Rehacerlas en español es una tarea que espera a una persona voluntaria: los pull requests son bienvenidos.

![image](../images/Вкладка-Текст/1.png)
Permite colocar imágenes de texto sobre la tira

## **Principio de funcionamiento**
- Seleccionas el área para el texto con `Shift+clic izq.`
- Se abre la ventana de edición, en la que se inserta el texto del globo que haya quedado dentro del área seleccionada
- Al perder el foco (clic fuera), la ventana de edición se cierra y se crea la imagen de texto con los parámetros indicados
- La imagen se puede mover arrastrándola 
- La imagen se puede escalar con las teclas `-` y `=`
- La imagen se puede girar poniendo el cursor encima y usando `Ctrl+rueda del ratón`
- El tamaño de la fuente se puede regular con `Shift+rueda del ratón`

## **Panel de parámetros**
### **Vista previa del texto**
![image](../images/Вкладка-Текст/2.png)

Muestra qué aspecto tendrá el texto con los parámetros actuales, sin tener en cuenta el tamaño de la fuente. Se actualiza automáticamente.

### **Parámetros principales del texto**
![image](../images/Вкладка-Текст/2_1.png)

- `Preajuste`: guarde y cargue los parámetros establecidos para cada fuente.
- `Fuente`: el programa incluye varias fuentes, que se muestran en la lista desplegable. Puede añadir las suyas propias echando un archivo ttf/otf en la carpeta fonts
- `face`: elección de la fuente dentro de la familia, si hay más de una
- `Usar fuentes del sistema`: por defecto las fuentes se cargan solo de la carpeta fonts que está junto a la instalación del programa
- `Grupo de fuentes`: limite las fuentes mostradas a las que están en `fonts/groups/<nombre del grupo>`. Las fuentes se pueden duplicar.
- `Tamaño`: tamaño de la fuente
- `Interlineado`: espacio entre las líneas de texto en píxeles. Puede ser negativo.
- `Kerning`: `Métrico`: distancia siempre igual entre las letras. `Auto`: usar los pares de caracteres de la fuente, por ejemplo `AV`, para que algunos caracteres queden más juntos. No funciona con todas las fuentes.
- `Kerning X`: distancia adicional entre caracteres
- `Alto/Ancho de carácter`
- `Alineación`: a la izquierda, a la derecha, centrada o libre. Se puede mover el deslizador para una posición intermedia.
- `Rotación global`: gira todo el texto mientras aún está en estado vectorial (sin rasterizar). Da una imagen notablemente más nítida que la rotación normal.
- `Forma`: principio de composición de las líneas; más detalles abajo.
- `Ajuste`: partición automática del texto para respetar la forma. Regula su intensidad o la desactiva.
- `Suavizado`: igual que el suavizado en Photoshop. Afecta a la nitidez del texto.
- `Permitir espiga moderada`: afecta a la forma del texto. Permite entrantes en la forma.
- `Bold`: **fuente en negrita**, no funciona con todas las fuentes
- `Italic`: fuente inclinada, no funciona con todas
- `Puntuación colgante`: los signos de puntuación no se ven afectados automáticamente por la alineación del texto. La lista de caracteres está en los ajustes, pero allí están casi todos.
- `Eliminar espacios sobrantes`: elimina los espacios en los extremos de las líneas.
- `Nueva línea tras el fin de la oración`
- `Todo en mayúsculas`
- `Analizar etiquetas`: permite marcar una parte concreta del texto con `<b>` o `<i>` para que solo esa parte tenga ese estilo. Ejemplo: `En negrita irá solo <b>una</b> palabra`. Ahora esto está automatizado: basta con seleccionar el texto, cambiar los parámetros disponibles, y se envolverá en etiquetas.

### **Parámetros avanzados del texto**
![image](../images/Вкладка-Текст/2_2.png)

- `Línea`: puede ser horizontal (lo habitual) o vertical.
  - Para la vertical está disponible tanto de derecha a izquierda como de izquierda a derecha.
- `Disposición de líneas`: normal y por fórmula. La fórmula permite hacer cualquier forma.

### **Parámetros adicionales del texto**
No siempre están disponibles.

#### **Parámetros solo para el texto seleccionado**
- `Desplazamiento X/Y`: desplazamiento del carácter o del grupo
- `Desplazamiento por la línea`: si el texto usa una disposición personalizada o por fórmula, desplaza los caracteres seleccionados a lo largo de ella
  - `Desplazar los caracteres siguientes`: solo para el desplazamiento por la línea. El texto siguiente se moverá junto con el fragmento seleccionado.
- `Rotación del carácter`: en el texto seleccionado, gira cada carácter por separado
- `Rotación del grupo`: gira junto todo el texto seleccionado
- `No dividir`: el texto no se partirá en la partición automática.


## **Panel de edición de texto**
![image](../images/Вкладка-Текст/13.png)


Aparece al seleccionar una imagen de texto. Se parece al panel de creación de texto, pero tiene un campo de texto. Los cambios se aplican al instante.

**Se puede seleccionar una parte del texto y cambiar los parámetros disponibles, y eso creará automáticamente etiquetas en línea.**


## **Forma del texto**
Aunque sería más correcto llamarla forma rápida.

El texto intenta encajar en esta forma, evitando espigas y usando una partición inteligente.

El texto puede tener estas formas:
- `Libre`: se coloca como de costumbre.
- `[ ]`: cuadrada
- `( )`: ovalada
- `< >`: hexagonal
- Las 2 últimas formas tienen un parámetro de ancho mínimo. Cuanto mayor sea, más anchos serán la parte superior e inferior del texto.

![image](../images/Вкладка-Текст/14.png)![image](../images/Вкладка-Текст/15.png)![image](../images/Вкладка-Текст/16.png)

Como se ve, las formas no se diferencian mucho. Pero es mejor que use la ovalada.
A veces la fuerza de la partición no basta para encajar el texto en la forma. Entonces juegue con los parámetros de ancho, tamaño de fuente, ancho mínimo y fuerza de la partición. O elija una forma rápida haciendo clic derecho sobre la capa de texto.

## **Forma de texto avanzada**
![image](../images/Вкладка-Текст/21_1.png)
![image](../images/Вкладка-Текст/21_2.png)

Se encuentra en el panel de edición.
Muestra desde unas pocas hasta cientos de miles de formas posibles, sin espigas y con la partición correcta, que se pueden filtrar.
A diferencia de la forma rápida, esta forma es estable y no cambiará hasta que la cambie usted mismo o elija otra. Se conserva al cambiar la fuente y otros parámetros.

### Texto inicial y texto formado
Cuando se aplica la forma avanzada, para crear la capa de texto el programa ya no toma el texto inicial, sino el formado. Se puede alternar entre ambos. El texto formado se puede editar a mano y añadirle etiquetas. Pero al volver a elegir una forma, el programa tomará de nuevo el texto inicial para calcularla, y al elegir otra forma el texto formado se sobrescribirá.


## **Efectos de texto**

### **Contorno**
![image](../images/Вкладка-Текст/3.png)![image](../images/Вкладка-Текст/4.png)

Rodea el texto con una línea del color y grosor indicados

### **Resplandor**
![image](../images/Вкладка-Текст/5.png)![image](../images/Вкладка-Текст/6.png)

Crea un aura alrededor del texto

### **Sombra**
![image](../images/Вкладка-Текст/7.png)![image](../images/Вкладка-Текст/8.png)

Añade al texto una sombra con el desplazamiento indicado en X e Y

### **Degradado: 2 colores**
![image](../images/Вкладка-Текст/9.png)![image](../images/Вкладка-Текст/10.png)

Hace el texto degradado en la dirección deseada

### **Degradado: 4 esquinas**
![image](../images/Вкладка-Текст/11.png)![image](../images/Вкладка-Текст/12.png)

Añade al texto un degradado basado en cuatro esquinas

### **Acciones**
- `Actualizar la imagen original`: recarga la página actual en el lienzo. Hace falta si se olvidó de limpiar algo.
- `Mostrar los globos de texto`: posibilidad de ocultar los globos de texto laterales para valorar mejor la traducción final

### **Guardado**
Guarda la serie traducida de dos maneras. Se recomienda el render de escena.


## **Máscara de recorte**
![image](../images/Вкладка-Текст/17.png)![image](../images/Вкладка-Текст/18.png)
![image](../images/Вкладка-Текст/19.png)
![image](../images/Вкладка-Текст/20.png)

Permite recortar las imágenes de texto. Si una imagen de texto toca la máscara, será recortada y solo se verán las partes que quedan bajo la máscara. Para cada texto se puede desactivar el recorte en el menú del clic derecho.
Tiene un color amarillo translúcido y no se ve cuando este panel está cerrado.

### **Pincel de la máscara**

- Está activado por defecto mientras el panel está abierto
- Con el `clic izq.` se dibuja, con el `clic der.` o `Shift+clic izq.` se borra. El tamaño se regula con `Ctrl+rueda`.

### **Relleno de la máscara**

- Se activa al pulsar el botón correspondiente en el panel de la máscara
- Con un clic izquierdo empezará a rellenar desde el punto del clic, extendiéndose por el color similar teniendo en cuenta la tolerancia


## **Transformación del texto (perspectiva)**
![image](../images/Вкладка-Текст/22.png)![image](../images/Вкладка-Текст/22_1.png)

Se puede entrar en el modo de transformación pulsando el clic derecho sobre el texto seleccionado y eligiendo la opción correspondiente del menú.
Permite deformar el texto no solo para la perspectiva, arrastrándolo por los tiradores.


## **Disposición de texto personalizada**
![image](../images/Вкладка-Текст/23_1.png)![image](../images/Вкладка-Текст/23_2.png)![image](../images/Вкладка-Текст/23_3.png)

Algo muy útil para las onomatopeyas.
**Para empezar, elija en el menú del clic derecho del texto la opción "Disposición de texto personalizada"**

**Este modo no permite perder el foco sobre el texto, hay que pulsar "Salir" de forma intencionada**

- Cambie el tamaño del texto arrastrando el área por las esquinas
- Tras elegir una línea en el panel, pulse el clic izquierdo dentro del área para añadir su comienzo
- Con Shift+clic izq. arrastre el punto cuadrado, dejando puntos intermedios para definir la forma de la línea
- Con el clic izq. simplemente arrastre los puntos, cambiando la forma sin crear otros nuevos
  - El punto redondo grande es el comienzo de la línea
  - El pequeño redondo es intermedio
  - El punto cuadrado es el final, y por él se puede alargar la línea
  - Si los puntos y la línea están en gris, es que ahora hay seleccionada otra línea
    - Si todos los puntos y líneas están en gris, es que la línea seleccionada todavía no se ha creado; pulse el clic izquierdo para crear el primer punto.

### **Mecánica**
- A cada línea de esta disposición le corresponde el fragmento de texto que hay entre saltos de línea. De una vez se pueden crear varias onomatopeyas parecidas

### **Panel**
![image](../images/Вкладка-Текст/23_4.png)

- Añadir y eliminar líneas
- Suavizado (hace que la línea no sea angulosa)
- Dirección y volteo del texto
- Distancia mínima al carácter: simplemente a lo largo de la línea, o sin permitir que los caracteres se solapen


## **Añadir fuentes propias**
En la carpeta del programa hay una carpeta `fonts`, donde están las 4 fuentes principales: Anime Ace, la de los rótulos, la de las onomatopeyas y Arial. Ahí puede echar sus archivos `.ttf`/`.otf`.


## **Disposición de texto por fórmula**
No sé para qué, pero existe.
La disposición por fórmula está en `Parámetros avanzados`:
- `Disposición`: `Normal` o `Fórmula`
- línea de fórmulas `x`, `y`, `rotation`
- botón `?` después de las fórmulas (mostrar/ocultar la chuleta de variables y funciones)
- parámetros de la trayectoria (`t_start/t_end`, `offset`, `scale`, `normal_offset`, `letter_spacing`)
- constantes `a..h`

### **Cómo funciona**
El programa calcula la posición y el ángulo **de cada carácter por separado**.

Para cada carácter se toma el parámetro `t` (normalmente de `0` a `1`), y después:
- `x = formula_x(...)`
- `y = formula_y(...)`
- `rotation = formula_rotation(...)` (en radianes)

Después se aplican los desplazamientos/escalas y, si está activado, la rotación tangente a la trayectoria.

### **Parámetros del modo por fórmula**
| Parámetro | Qué hace | Cómo usarlo |
|---|---|---|
| `Disposición` | Cambia el modo | `Normal` para la disposición estándar, `Fórmula` para arcos/espirales/trayectorias arbitrarias |
| `x` | Fórmula de la coordenada X de cada carácter | Punto de partida básico: `t * w` |
| `y` | Fórmula de la coordenada Y | Punto de partida básico: `120 * sin((t - 0.5) * pi)` |
| `rotation` | Rotación adicional de cada carácter | `0` para no añadir rotación, `0.2*sin(2*pi*t)` para una onda |
| `?` | Muestra/oculta la ayuda | Cómodo cuando hay que recordar rápidamente las variables/funciones |
| `Rotación tangente` | Gira el carácter siguiendo la dirección de la curva | Actívela para arcos/espirales, desactívela para un texto «recto» sobre la curva |
| `t_start` | Inicio del rango del parámetro `t` | Normalmente `0` |
| `t_end` | Fin del rango de `t` | Normalmente `1`; auméntelo para una «mayor longitud» de la curva |
| `offset_x` | Desplazamiento de toda la trayectoria en X (px) | Mover todo el rótulo a la derecha/izquierda |
| `offset_y` | Desplazamiento de toda la trayectoria en Y (px) | Mover todo el rótulo arriba/abajo |
| `scale_x` | Escala de la trayectoria en X | `>1` estira, `<1` comprime |
| `scale_y` | Escala de la trayectoria en Y | Controla la amplitud vertical |
| `normal_offset` | Desplazamiento de los caracteres según la normal a la curva | Útil para sacar el texto hacia fuera/dentro de una circunferencia |
| `letter_spacing` | Multiplicador de la distancia entre caracteres | `1` normal, `>1` separa, `<1` aprieta |
| `a..h` | Constantes de usuario para las fórmulas | Guarde en ellas los «mandos» de amplitud, radio, número de vueltas, etc. Pero son solo números, no se puede introducir una fórmula en ellas. |

### **Variables en las fórmulas**
- `t`: posición actual dentro del rango (`t_start..t_end`)
- `u`: posición centrada (`-1..1`)
- `i`: índice del carácter
- `n`: número de caracteres
- `s`: longitud acumulada a lo largo de la línea (en píxeles)
- `line`: índice de la línea
- `line_t`: posición del carácter dentro de la línea actual (`0..1`)
- `line_n`: número de caracteres en la línea actual
- `w` / `width`: ancho del bloque de texto (px)
- `fs` / `font_size`: tamaño de la fuente
- `a..h`: sus constantes de la interfaz
- `pi`, `tau`, `math_e`: constantes matemáticas

### **Funciones disponibles**
`sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `sqrt`, `abs`, `exp`, `ln`, `log`, `min`, `max`, `clamp`, `pow`, `rad`, `deg`, `floor`, `ceil`, `round`, `sign`

### **Importante sobre el ángulo**
- `rotation` se indica en **radianes**
- si le resulta más cómodo en grados, use `rad(grados)`, por ejemplo: `rad(25)`

### **Ejemplos listos (cópielos tal cual)**
#### 1) Arco
- `x`: `t * w`
- `y`: `120 * sin((t - 0.5) * pi)`
- `rotation`: `0`
- `Rotación tangente`: `activada`

#### 2) Línea inclinada
- `x`: `t * w`
- `y`: `0.35 * t * w`
- `rotation`: `0`
- `Rotación tangente`: `desactivada`

#### 3) Onda
- `x`: `t * w`
- `y`: `80 * sin(2 * pi * t)`
- `rotation`: `0.15 * sin(2 * pi * t)`
- `Rotación tangente`: `desactivada`

#### 4) Espiral (mediante `a`, `b`, `c`)
- `a = 40`, `b = 180`, `c = 3`
- `x`: `(a + b * t) * cos(c * tau * t)`
- `y`: `(a + b * t) * sin(c * tau * t)`
- `rotation`: `0`
- `Rotación tangente`: `activada`

#### 5) Exponencial
- `a = 3`
- `x`: `t * w`
- `y`: `140 * (exp(a * t) - 1) / (exp(a) - 1)`
- `rotation`: `0`
- `Rotación tangente`: `activada`

### **Inicio rápido (para no romper la composición)**
Ponga los valores básicos:
- `t_start = 0`
- `t_end = 1`
- `offset_x = 0`
- `offset_y = 0`
- `scale_x = 1`
- `scale_y = 1`
- `normal_offset = 0`
- `letter_spacing = 1`

Y solo después cambie los parámetros de uno en uno.
