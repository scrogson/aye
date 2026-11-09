/*
 * pamtest SERVICE: runs PAM authentication for the current user against
 * /etc/pam.d/SERVICE and says whether it passed, like pamtester (which macOS
 * doesn't have). deploy/macos/pam.sh builds it to try a throwaway service.
 */
#include <pwd.h>
#include <stdio.h>
#include <unistd.h>

#include <security/pam_appl.h>
#ifdef __APPLE__
#include <security/openpam.h>
#define CONV openpam_ttyconv
#else
#include <security/pam_misc.h>
#define CONV misc_conv
#endif

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: pamtest SERVICE\n");
        return 2;
    }
    struct passwd *pw = getpwuid(getuid());
    if (!pw) return 2;
    struct pam_conv conv = {CONV, NULL};
    pam_handle_t *h;
    int rc = pam_start(argv[1], pw->pw_name, &conv, &h);
    if (rc != PAM_SUCCESS) {
        fprintf(stderr, "pam_start failed: %d\n", rc);
        return 2;
    }
    rc = pam_authenticate(h, 0);
    printf("pamtest: %s\n", rc == PAM_SUCCESS ? "authenticated" : pam_strerror(h, rc));
    pam_end(h, rc);
    return rc == PAM_SUCCESS ? 0 : 1;
}
