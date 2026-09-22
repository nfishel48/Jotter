# librispeech-test-clean

**WER 1.74%**  (95% CI 1.60%–1.90%)
2620 utterances · 53027 reference words

> **Working tree was dirty.** The recorded commit does not describe the code that produced this number.

## Where the errors are

| | count | share of errors |
| --- | ---: | ---: |
| substitutions | 692 | 75.0% |
| deletions | 133 | 14.4% |
| insertions | 98 | 10.6% |

**Substitutions dominate — 75.0% of all errors.** That points at the acoustic model: the words are being heard, and heard wrong.

## Cost

- 5.40 h of audio in 22.3 min
- real-time factor **0.069** (15× faster than real time)

## What produced this

- jotter 0.1.13 (transcribe v1) at `a45e1baaf295`
- model `parakeet-tdt-0.6b-v2-int8` via sherpa-onnx, 4 threads
- segmentation **none**
- normaliser whisper: transformers 4.53.2, english.json sha256:6607f948be98 (1739 entries)
- host macOS-26.3.1-arm64-arm-64bit-Mach-O

## Worst utterances

Ranked by absolute errors, not rate: a one-word utterance scored 100% tells you nothing, a thirty-word one scored 40% tells you a lot.

**4992-41797-0001** — 12 errors (14%)

- ref: well as i say it is an awful queer world they clap all the burglars into jail and the murderers and the wife beaters i have allers thought a gentle reproof would be enough punishment for a wife beater cause he probably has a lot 0 provocation that nobody knows and the firebugs can not think 0 the right name something like cendenaries an the breakers 0 the peace an what not an yet the law has nothin to say to a man like hen lord
- hyp: well as i say it is an awful queer world they clap all the burglars in jail and the murderers and the wife beaters i allers thought a gentle reproof would be enough punishment for a wife beater cause he probably has a lot of provocation that nobody knows and the firebugs can not think of the right name something like cendiaries and the breakers of the peace and what not and yet the law has nothing to say to a man like hanlord

**8555-284447-0002** — 9 errors (32%)

- ref: i would not mind a cup 0 coffee myself said cap n bill i have had consid ble exercise this mornin and i am all ready for breakfas
- hyp: i would not mind a cup of coffee myself said campbell i have had considerable exercise this morning and am all ready for breakfast

**121-123859-0002** — 6 errors (9%)

- ref: but reckoning time whose 1000000 would accidents creep in twixt vows and change decrees of kings tan sacred beauty blunt the sharp saint intents divert strong minds to the course of altering things alas why fearing of time is tyranny might i not then say now i love you best when i was certain 0 er incertainty crowning the present doubting of the rest
- hyp: but reckoning time whose 1000000 would accidents creep in twixt vows and change decrees of kings tan is sacred beauty blunt the sharpest intents diverts strong minds to the course of altering things alas why fearing of time is tyranny might i not then say now i love you best when i was certain 0 er in certainty crowning the present doubting of the rest

**1995-1836-0004** — 6 errors (6%)

- ref: as she awaited her guests she surveyed the table with both satisfaction and disquietude for her social functions were few tonight there were she checked them off on her fingers sir james creighton the rich english manufacturer and lady creighton mister and missus vanderpool mister harry cresswell and his sister john taylor and his sister and mister charles smith whom the evening papers mentioned as likely to be united states senator from new jersey a selection of guests that had been determined unknown to the hostess by the meeting of cotton interests earlier in the day
- hyp: as she awaited her guests she surveyed the table with both satisfaction and disquietude for her social functions were few to night there were she checked them off on her fingers sir james crichton the rich english manufacturer and lady crichton mister and misses vanderpoel mister harry cresswell and his sister john taylor and his sister and mister charles smith whom the evening papers mentioned as likely to be united states senator from new jersey a selection of guests that had been determined unknown to the hostess by the meeting of cotton interests earlier in the day

**2094-142345-0027** — 6 errors (40%)

- ref: munny i tould ike to do into de barn to tommy to see de whittawd
- hyp: money i d uke to do into the barn to tommy to see the widod

**4992-41806-0014** — 6 errors (13%)

- ref: thinks i to myself i never seen anything osh popham could not mend if he took time enough and glue enough so i carried this little feller home in a bushel basket one night last month an i have spent 11 evenin is puttin him together
- hyp: thinks i to myself i never seen anything osh pop em could not mend if he took time enough and glue enough so i carried this little feller home in a bushel basket one night last month and i have spent 11 evenins putting him together

**1089-134691-0017** — 5 errors (17%)

- ref: the europe they had come from lay out there beyond the irish sea europe of strange tongues and valleyed and woodbegirt and citadelled and of entrenched and marshaled races
- hyp: the europe they had come from lay out there beyond the irish sea europe of strange tongues and valley and wood begirt and citadeled and of entrenched and martialed races

**61-70968-0034** — 5 errors (62%)

- ref: a montfichet a montfichet gamewell to the rescue
- hyp: amontfichet amontfichet game well to the rescue

**7127-75947-0033** — 5 errors (25%)

- ref: how is it la valliere said mademoiselle de tonnay charente that the vicomte de bragelonne spoke of you as louise
- hyp: how is it lavalier said mademoiselle de tenechant that the vicomte de bragalone spoke of you as louise

**908-157963-0007** — 5 errors (6%)

- ref: the lilly of the valley breathing in the humble grass answerd the lovely maid and said i am a watry weed and i am very small and love to dwell in lowly vales so weak the gilded butterfly scarce perches on my head yet i am visited from heaven and he that smiles on all walks in the valley and each morn over me spreads his hand saying rejoice thou humble grass thou new born lily flower
- hyp: the lily of the valley breathing in the humble grass answered the lovely maid and said i am a watery weed and i am very small and love to dwell in lowly vales so weak the gilded butterfly scarce perches on my head yet i am visited from heaven and he that smiles on all walks in the valley and each morn over me spreads his hand saying rejoice thou humble grass thou newborn lily flower
