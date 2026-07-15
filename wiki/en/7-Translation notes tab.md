# **Translation notes tab**

**Note:** the screenshots are captured with the Russian interface. Retaking them in English is a task waiting for a volunteer — pull requests are welcome.

Builds the request for an AI translation, using your instructions and inserting the list of characters and terms in the right place.

## **Composed prompt**
The tab with the final, non-editable prompt, which you can copy, or toggle the insertion of the characters and the terms.


## **Template**
The tab where you can edit your own instructions. 
**!Be sure to insert the placeholders!**
- `{charas}` - the characters will be inserted in its place
- `{terms}` - the terms will be inserted in its place

## **Example**

```
Please help me translate a webtoon from Korean into English.
# Translation rules

- I will give you recognized text, there may be inaccuracies from OCR, but more often it is simply missing spaces.
- Think carefully about the translation options, then write the final version, clearly separating the lines. 
  - In the translation write the roles, but without quotes and extra descriptions. Only the translated lines
  - A line on a new line comes from the same character; a line after 2 new lines comes from a different character. The character is written after their lines.
  - One line in `` is one speech balloon in the comic, take that into account and keep the structure. Never merge two lines into one, do not invent new ones, and try not to make lines too long if the original one was short.
- After the translation, if there is a new term or character, write notes about them. In the case of a new term, also write the original name.
- Try to adapt the translation creatively for an English-speaking audience, insert familiar slang and memes, you may change the intonation and the rudeness of the lines to make the translation livelier, but never change the meaning of a line drastically.
- Funny ad-libbing in the main character's thoughts is welcome, that is exactly where you should add more jokes. The main thing is not to overdo it and not to turn it into a swearing contest. The thoughts of an ordinary 20-year-old guy.
- Do not insert address suffixes like "–씨" (translated as -ssi) into the translation unnecessarily, unless it matters for understanding.

# **Story context**

{charas}

{terms}
```
