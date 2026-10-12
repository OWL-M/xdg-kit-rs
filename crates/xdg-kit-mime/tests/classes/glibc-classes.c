/* Writes glibc-c-utf8.txt, the reference the glob class test in
   src/fnmatch.rs compares with: for each POSIX class, the Unicode scalar
   values glibc's iswctype puts in it under the locale named by argv[1],
   as hexadecimal ranges, wrapped at 80 columns. It also checks that
   fnmatch("[[:class:]]", c, 0) agrees with iswctype for each value.

   From the repository root, with the locked nixpkgs' gcc and locales:

     locales=$(nix build --inputs-from . --no-link --print-out-paths \
       nixpkgs#glibcLocales)
     nix shell --inputs-from . nixpkgs#gcc -c \
       gcc -O2 -Wall -o /tmp/glibc-classes \
       crates/xdg-kit-mime/tests/classes/glibc-classes.c
     LOCALE_ARCHIVE=$locales/lib/locale/locale-archive \
       /tmp/glibc-classes C.UTF-8 \
       > crates/xdg-kit-mime/tests/classes/glibc-c-utf8.txt

   en_US.UTF-8 gives the same classes (both copy i18n_ctype). */
#include <fnmatch.h>
#include <gnu/libc-version.h>
#include <locale.h>
#include <stdio.h>
#include <string.h>
#include <wctype.h>

static const char *classes[] = {"alnum", "alpha", "blank", "cntrl",
  "digit", "graph", "lower", "print", "punct", "space", "upper", "xdigit"};

static void utf8(unsigned c, char *out) {
  if (c < 0x80) {
    *out++ = c;
  } else if (c < 0x800) {
    *out++ = 0xc0 | c >> 6;
    *out++ = 0x80 | (c & 63);
  } else if (c < 0x10000) {
    *out++ = 0xe0 | c >> 12;
    *out++ = 0x80 | (c >> 6 & 63);
    *out++ = 0x80 | (c & 63);
  } else {
    *out++ = 0xf0 | c >> 18;
    *out++ = 0x80 | (c >> 12 & 63);
    *out++ = 0x80 | (c >> 6 & 63);
    *out++ = 0x80 | (c & 63);
  }
  *out = 0;
}

/* Prints one range of `name`, starting a new line past 80 columns. */
static void range(const char *name, unsigned low, unsigned high, int *column) {
  char text[32];
  if (low == high)
    snprintf(text, sizeof text, " %X", low);
  else
    snprintf(text, sizeof text, " %X-%X", low, high);
  int width = strlen(text);
  if (*column < 0 || *column + width > 80) {
    if (*column >= 0)
      putchar('\n');
    *column = printf("%s", name);
  }
  *column += printf("%s", text);
}

int main(int argc, char **argv) {
  if (argc != 2 || !setlocale(LC_ALL, argv[1])) {
    fprintf(stderr, "usage: glibc-classes LOCALE (an installed one)\n");
    return 1;
  }
  printf("# glibc %s, locale %s: iswctype per class, hexadecimal ranges.\n",
         gnu_get_libc_version(), argv[1]);
  printf("# Written by glibc-classes.c; see there to regenerate.\n");
  long disagree = 0;
  for (unsigned k = 0; k < sizeof classes / sizeof *classes; k++) {
    wctype_t type = wctype(classes[k]);
    char pattern[32];
    snprintf(pattern, sizeof pattern, "[[:%s:]]", classes[k]);
    long start = -1;
    int column = -1;
    for (unsigned c = 0; c <= 0x110000; c++) {
      int in = 0;
      /* Surrogates are not scalar values: they end a range. */
      if (c < 0x110000 && !(c >= 0xd800 && c <= 0xdfff)) {
        in = iswctype(c, type) != 0;
        char text[8];
        utf8(c, text);
        /* NUL cannot be in a C string: no fnmatch check for it. */
        if (c != 0 && (fnmatch(pattern, text, 0) == 0) != in) {
          disagree++;
          fprintf(stderr, "fnmatch and iswctype differ: %s U+%04X\n",
                  classes[k], c);
        }
      }
      if (in && start < 0)
        start = c;
      if (!in && start >= 0) {
        range(classes[k], start, c - 1, &column);
        start = -1;
      }
    }
    putchar('\n');
  }
  if (disagree) {
    fprintf(stderr, "%ld fnmatch disagreements\n", disagree);
    return 1;
  }
  return 0;
}
