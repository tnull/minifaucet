import random

with open('bip39-wordlist.txt') as f:
    lines = f.read().splitlines()
    fout = open("passphrases.txt", "a")
    for _ in range(0, 30):
        passphrase = random.choice(lines) + "-" + random.choice(lines) + "-" + random.choice(lines)
        print(passphrase)
        fout.write(passphrase + "\n")

