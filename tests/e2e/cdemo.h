typedef unsigned char byte;

struct pair {
    int a;
    int b;
};

enum shade {
    SH_RED,
    SH_GREEN = 5
};

#define CDEMO_LIMIT 42

#define CD_DOUBLE(x) ((x) * 2)

int cdemo_shared;
