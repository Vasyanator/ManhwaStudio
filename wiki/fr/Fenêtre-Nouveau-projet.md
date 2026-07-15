# Fenêtre **Nouveau Projet**

**Remarque :** les captures d'écran sont prises avec l'interface en russe. Les refaire en français est une tâche qui attend une personne volontaire — les pull requests sont les bienvenues.

![image](../images/Окно-Новый-проект/1.png)

Télécharge un chapitre depuis différents sites et le pré-traite.

## Traitement par lots
Téléchargement et traitement en masse de chapitres à partir d'un graphe de nœuds. Encore inachevé et sans finition. Fonctionne partiellement. N'y prêtez pas attention.


## **Import**
Le bouton `Ouvrir un dossier` permet d'ouvrir un dossier contenant les images d'un manhwa et de les importer.

- Vous pouvez ouvrir un dossier contenant un chapitre déjà téléchargé ; dans ce cas, les images doivent être nommées dans le bon ordre, par exemple `1.png/jpg/jpeg`
- Vous pouvez ouvrir un site avec le chapitre enregistré depuis votre navigateur principal. 
  - Dans ce cas, le programme examine le fichier `html` situé un niveau au-dessus et portant le nom du dossier, et charge les images dans le même ordre que sur la page.
  - Si le fichier HTML est introuvable, le programme essaiera de charger les images ou les `resource(X)` comme des images, dans l'ordre des noms.
  - Vous pouvez définir un motif de nom de fichier si les fichiers d'images portent des noms inhabituels
- Le filtre à ±50 % de largeur fonctionne bien avec les bandes dessinées au format vertical, en aidant à retirer les images de publicité, mais **il vaut mieux le désactiver pour le manga et les autres bandes dessinées paginées**, sinon des pages risquent de disparaître

Le bouton `Ouvrir un fichier` permet d'ouvrir une image isolée, une archive, ou un fichier html d'un site téléchargé.

Le bouton `Coller depuis le presse-papiers` permet de coller une image copiée.

`Mode d'ajout` — activez-le pour ne pas effacer complètement la bande lorsque vous ajoutez une image oubliée.

## **Téléchargeur rapide**
![image](../images/Окно-Новый-проект/2.png)

- Le champ de saisie en haut et le bouton de téléchargement permettent de télécharger rapidement un chapitre gratuit depuis comic.naver.com, **! Pas depuis series.naver.com !**
- Survolez le bouton de téléchargement pour voir les sites pris en charge.


## **Téléchargeur avancé**
![image](../images/Окно-Новый-проект/3_1.png)
![image](../images/Окно-Новый-проект/3_2.png)
![image](../images/Окно-Новый-проект/3_3.png)

Ouvre la page indiquée dans un vrai navigateur et télécharge les images selon la méthode choisie.

### **Interception profonde**
Le mode le plus simple et le plus universel, il fonctionne même avec des sites complexes. Mais il **ne fonctionne qu'avec CloakBrowser**, et il télécharge depuis la page tout ce qui ressemble à une image. **Une fois son travail terminé, une fenêtre s'ouvrira et il faudra désactiver manuellement les images qui ne relèvent pas du chapitre, par exemple les publicités.**

## **Télécharger le Canvas depuis la page**
Sa fonctionnalité est déjà intégrée à l'interception profonde, on peut ne pas y toucher. Télécharge les images dans le cas où ce sont des `<canvas>` et non des `<img>`.

## **Recherche de liens par motif**
Méthode plus propre, mais pénible, qui ne fonctionne pas partout. **IL FAUT DES BASES POUR FOUILLER DANS LE CODE DE LA PAGE**, guide au bas de ce wiki.

Recherche les liens selon un modèle de préfixe :
- `*` signifie n'importe quelle combinaison de caractères
- `?` signifie un seul caractère quelconque
- C'est un préfixe, donc son début compte. La fin instable peut être omise.

Les préfixes peuvent être enregistrés et chargés.

### Collecte des liens
Aide si toutes les images ne sont pas apparues d'un coup sur la page. Par exemple si le site les charge au fur et à mesure, ou s'il s'agit d'une liseuse page par page.

**Dans ce cas, lancez la collecte, faites défiler tout le chapitre, puis arrêtez la collecte.**

### Threads de téléchargement
Le téléchargement multithread est bien plus rapide, mais il ne fonctionne pas toujours. Si les images doivent être récupérées en utilisant la session du navigateur et non une requête ordinaire, le téléchargement est hélas monothread.


## **Assemblage / Découpe**
![image](../images/Окно-Новый-проект/4.png)

Assemble toutes les images en une seule bande, puis les redécoupe de façon à ne couper ni le texte ni le dessin. **! À ne pas utiliser pour le manga !**, seulement pour les manhwa/manhua et autres bandes dessinées en forme de longue bande.

### **Paramètres de l'assemblage**
- `Nombre de parties` : en combien de parties découper la bande. Si vide, c'est automatique.
- `Hmax` : à quelle hauteur (en pixels) découper les parties de la bande lors de la découpe automatique.
- `Bande blanche` : sur combien de pixels de hauteur vérifier l'uniformité de la couleur lors du repérage des endroits de coupe. Plus simplement : quelle épaisseur minimale doit avoir une bande d'une seule couleur pour qu'on puisse y couper.
- `Tolérance d'uniformité` : de combien la couleur des pixels peut varier à un endroit où l'on peut couper. Il vaut mieux l'augmenter s'il s'agit d'un shôjo avec plein de belles illustrations.
- `search radius` : jusqu'où, de part et d'autre de l'endroit de coupe prévu, chercher un endroit approprié.

### **Modes de fonctionnement**
- `Assembler la bande` — assemble simplement en une seule longue bande et rien de plus
- `Assembler et placer les lignes de coupe` — assemble et marque les endroits de découpe pour un contrôle manuel. Voir ci-dessous.
- `Assembler et découper automatiquement` — assemble et découpe immédiatement aux endroits optimaux. Rapide, mais le contrôle manuel est préférable.
- `Assembler uniquement aux endroits irréguliers` — ne découpe pas, mais recolle la bande uniquement là où les coupures tombaient au milieu d'un dessin ou d'une texture

### **Assemblage et découpe manuels**
Après `Assembler et placer les lignes de coupe`, ou après l'ajout manuel d'une ligne de coupe, cette interface apparaît :
![image](../images/Окно-Новый-проект/4_5.png)
  - **La flèche rouge** marque la ligne de coupe sur la barre de défilement
  - **La flèche bleue** marque une **coupe déjà existante**
  - **La ligne rouge** est la future coupe elle-même, on peut la déplacer et la supprimer
  - **Le bouton rouge** `Découper` en haut applique tous les points de coupe et réassemble la bande

- Une ligne de coupe peut être ajoutée depuis le menu du clic droit
- Depuis ce même menu du clic droit, on peut aussi assembler la page actuelle avec la suivante et la précédente

### **Autres actions sur la page**
![image](../images/Окно-Новый-проект/4_6.png)

C'est le menu d'actions dans le coin de chaque page.
- Les flèches haut et bas échangent la page actuelle avec la suivante ou la précédente
- La croix la supprime
- On peut recadrer la page manuellement


## **Découper en chapitre**
![image](../images/Окно-Новый-проект/4_1.png)

Prend pour base le chapitre sélectionné et découpe les images exactement de la même manière. Nécessaire pour télécharger des versions alternatives destinées à l'outil Tampon.

S'il y a une différence dans la hauteur totale des deux chapitres, une fenêtre s'ouvre :

![image](../images/Окно-Новый-проект/4_2.png)
![image](../images/Окно-Новый-проект/4_3.png)

Ici, il faut s'assurer que les images coïncident. L'image du chapitre téléchargé sera semi-transparente. Il faut ajuster la hauteur de façon à obtenir le résultat de la première image, et non celui de la seconde.

### **Ensuite, il faut enregistrer comme version alternative du chapitre sélectionné, en indiquant un nom.**


## **Traitement des images (Waifu2x/Reline)**
![image](../images/Окно-Новый-проект/5.png)

## Waifu2x

IA dépassée, mais toujours fonctionnelle, pour le débruitage et l'agrandissement. Plus simple et plus rapide que Reline

## Reline

IA moderne pour le débruitage et l'agrandissement. Elle possède de nombreux modèles, principalement pour le manga. 


## **Enregistrement**
![image](../images/Окно-Новый-проект/6.png)

Enregistre la série traitée dans la structure du projet ou simplement dans un dossier choisi (enregistrement indépendant).

Si vous enregistrez simplement le premier chapitre, choisissez « Enregistrer comme base de projet », saisissez un nom, et cliquez sur « Enregistrer et ouvrir ».

- La série est à la fois un champ de saisie et une liste déroulante. Vous pouvez saisir la vôtre.


# Décortiquer un site et créer un préfixe
Exemple avec mto.to

## 1. On ouvre le chapitre dans un navigateur ordinaire et on appuie sur F12
![image](../images/Окно-Новый-проект/7.png)

## 2. On survole différentes balises HTML et le navigateur montre lui-même à quoi elles correspondent. Si la partie du site contenant l'image du chapitre est mise en évidence, on déplie la balise jusqu'à atteindre l'image elle-même.
![image](../images/Окно-Новый-проект/8.png)

## 3. On ouvre la balise de l'image concrète et on regarde quel lien s'y trouve.
![image](../images/Окно-Новый-проект/9.png)
### Par exemple, ici nous avons le lien `https://n27.mbeaj.org/media/mbch/a97/6921b1dc4b5d85970424179a/128472992_800_14755_1072554.webp` On l'ouvre dans un nouvel onglet et on vérifie qu'il s'agit bien d'une image.

### Ensuite, on ouvre encore quelques balises d'images et on collecte les liens. Par exemple, voici :
- `https://n27.mbeaj.org/media/mbch/a97/6921b1dc4b5d85970424179a/128472992_800_14755_1072554.webp`
- `https://n25.mbuul.org/media/mbch/a97/6921b1dc4b5d85970424179a/128472994_800_12860_1448870.webp`
- `https://n21.mbrtz.org/media/mbch/a97/6921b1dc4b5d85970424179a/128473001_800_15000_1578696.webp`
- `https://n06.mbwww.org/media/mbch/a97/6921b1dc4b5d85970424179a/128473003_800_15000_1167770.webp`

## 4. On regarde attentivement les liens et on cherche ce qu'ils ont en commun. Par exemple :
- Par exemple, le sous-domaine commence toujours par n
- Le nom des sites contient toujours mb
- La première section est toujours /media
- Le reste, par exemple `mbch/a97/6921b1dc4b5d85970424179a`, peut changer d'une série à l'autre

## 5. On se rappelle comment fonctionne mon modèle simplifié
- `*` signifie n'importe quelle combinaison de caractères
- `?` signifie un seul caractère quelconque

## 6. On compose le modèle de préfixe
- On prend le début du lien, ici `https://n06.mbwww.org/media/`
- On remplace tout ce qui change par des caractères de substitution, par exemple à la place de `n06` il y aura `n*` ou `n??`
- On ajoute * à la fin
- On obtient quelque chose comme : `https://n*.mb*.org/media/*`

## 7. Félicitations ! `https://n*.mb*.org/media/*` peut être collé comme préfixe dans le téléchargeur avancé