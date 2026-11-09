/*
 * pam_aye: runs /usr/local/bin/aye for sudo, the way pam_exec does
 * on Linux. macOS has no pam_exec, so this is the piece it's missing.
 *
 *   auth  sufficient  /usr/local/lib/pam/pam_aye.so
 *
 * aye asks ayed, waits for the phone, and exits 0 only on a signed
 * approval. Anything else (a deny, no answer, Enter, ayed not running)
 * returns PAM_IGNORE, so PAM carries on to the next line: the password.
 *
 * An optional module argument replaces the program's path. aye gets a
 * clean environment (PAM_USER, PAM_TYPE, PAM_SERVICE), stdin from /dev/null,
 * and sudo as its parent, so it can read the command sudo is about to run.
 */
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <security/pam_appl.h>
#include <security/pam_modules.h>

#define AYE_PATH "/usr/local/bin/aye"

static int run_aye(const char *prog, const char *user, const char *service) {
    char env_user[512], env_service[256];
    snprintf(env_user, sizeof env_user, "PAM_USER=%s", user);
    snprintf(env_service, sizeof env_service, "PAM_SERVICE=%s", service ? service : "");
    char *const envp[] = {env_user, "PAM_TYPE=auth", env_service, "PATH=/usr/bin:/bin", NULL};
    char *const argv[] = {(char *)prog, NULL};

    /* If the caller ignores SIGCHLD the child would be reaped behind our back
     * and waitpid would fail, so wait with the default handler in place. */
    struct sigaction dfl, old;
    memset(&dfl, 0, sizeof dfl);
    dfl.sa_handler = SIG_DFL;
    sigemptyset(&dfl.sa_mask);
    sigaction(SIGCHLD, &dfl, &old);

    int ok = 0;
    pid_t pid = fork();
    if (pid == 0) {
        int null = open("/dev/null", O_RDWR);
        if (null >= 0) {
            dup2(null, STDIN_FILENO);
            if (null > STDERR_FILENO) close(null);
        }
        execve(prog, argv, envp);
        _exit(127);
    } else if (pid > 0) {
        int status = 0;
        pid_t r;
        while ((r = waitpid(pid, &status, 0)) < 0 && errno == EINTR) {
        }
        ok = r == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0;
    }
    sigaction(SIGCHLD, &old, NULL);
    return ok;
}

PAM_EXTERN int pam_sm_authenticate(pam_handle_t *pamh, int flags, int argc, const char **argv) {
    (void)flags;
    const char *user = NULL;
    const void *service = NULL;
    if (pam_get_user(pamh, &user, NULL) != PAM_SUCCESS || user == NULL || *user == '\0')
        return PAM_IGNORE;
    pam_get_item(pamh, PAM_SERVICE, &service);
    const char *prog = argc > 0 && argv[0][0] == '/' ? argv[0] : AYE_PATH;
    return run_aye(prog, user, service) ? PAM_SUCCESS : PAM_IGNORE;
}

PAM_EXTERN int pam_sm_setcred(pam_handle_t *pamh, int flags, int argc, const char **argv) {
    (void)pamh; (void)flags; (void)argc; (void)argv;
    return PAM_IGNORE;
}
