"""Generate a library administrator hash offline, without command-line passwords."""

import getpass
import hashlib
import secrets
import sys


def main():
    if not sys.stdin.isatty():
        raise SystemExit("Run in an interactive terminal for masked password input.")

    password = getpass.getpass("Administrator password: ")
    confirmation = getpass.getpass("Confirm password: ")
    encoded = password.encode("utf-8")
    if not 1 <= len(encoded) <= 1024:
        raise SystemExit("Password must contain 1 to 1024 UTF-8 bytes.")
    if password != confirmation:
        raise SystemExit("Passwords do not match.")

    salt = secrets.token_bytes(16)
    key = hashlib.pbkdf2_hmac("sha256", encoded, salt, 600_000, dklen=32)
    print(f"pbkdf2-sha256$v1$600000${salt.hex()}${key.hex()}")


if __name__ == "__main__":
    main()
