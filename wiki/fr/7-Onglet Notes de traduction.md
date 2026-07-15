# **Onglet Notes de traduction**

**Remarque :** les captures d'écran sont prises avec l'interface en russe. Les refaire en français est une tâche qui attend une personne volontaire — les pull requests sont les bienvenues.

Constitue la requête de traduction par IA, en utilisant les instructions et en insérant au bon endroit la liste des personnages et des termes.

## **Prompt assemblé**
Onglet contenant le prompt final non modifiable, que l'on peut copier, et où l'on peut activer ou non l'insertion des personnages et des termes.


## **Modèle**
Onglet où vous pouvez modifier vos propres instructions. 
**! N'oubliez surtout pas d'insérer les espaces réservés !**
- `{charas}` — les personnages seront insérés à sa place
- `{terms}` — les termes seront insérés à sa place

## **Exemple**

```
Aide-moi s'il te plaît à traduire un webtoon du coréen vers le français.
# Règles de traduction

- Je vais te donner du texte reconnu, il peut y avoir des imprécisions dues à l'OCR, mais le plus souvent il manque simplement des espaces.
- Réfléchis attentivement aux variantes de traduction, puis écris la version finale en séparant clairement les lignes. 
  - Dans la traduction, écris les rôles, mais sans guillemets ni descriptions supplémentaires. Uniquement les lignes traduites
  - Une réplique sur une nouvelle ligne vient du même personnage, une réplique après 2 sauts de ligne vient d'un autre personnage. Le personnage est indiqué après ses répliques.
  - Une ligne entre `` correspond à une bulle de texte de la bande dessinée : tiens-en compte et conserve la structure. Ne fusionne en aucun cas deux répliques en une seule, n'en invente pas de nouvelles, et essaie de ne pas faire de répliques trop longues si l'originale était courte.
- Après la traduction, s'il y a un nouveau terme ou un nouveau personnage, prends-en des notes. Dans le cas d'un nouveau terme, écris aussi le nom original.
- Essaie d'adapter la traduction de façon créative pour le public francophone, d'y glisser de l'argot et des mèmes familiers ; tu peux changer l'intonation et la grossièreté des répliques pour que la traduction soit plus vivante, mais ne change en aucun cas radicalement le sens d'une réplique.
- Une touche personnelle drôle dans les pensées du héros est la bienvenue : c'est précisément là qu'il faut mettre le plus de blagues. L'essentiel est de ne pas en faire trop et de ne pas verser dans les insultes à rallonge. Les pensées d'un mec ordinaire de 20 ans.
- N'insère pas inutilement dans la traduction des suffixes d'adresse du type « –씨 » (que l'on traduit par -ssi) si ce n'est pas important pour la compréhension.

# **Contexte de l'histoire**

{charas}

{terms}
```