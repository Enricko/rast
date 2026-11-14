/*
 * RAST E2E fixture: a tiny GTK3 desktop app used to validate native Linux app
 * execution (one shared Xpra session per RAST user) and the legacy Docker
 * backend.
 *
 * Behaviour that the scenarios rely on:
 *   - window title is "RAST Test App" (or "<title> #N" for the Nth window);
 *   - a large label mirrors what is typed in the entry (input responsiveness:
 *     the screenshot changes while typing through the browser);
 *   - a "New Window" button at a deterministic position opens another window
 *     (multi-window taskbar/focus/resize/close tests);
 *   - "--windows N" opens N windows at startup (the launcher argv is fixed at
 *     install time, so the archive ships a wrapper that passes it);
 *   - "--title TEXT" overrides the title, "--hold 1" keeps the app running
 *     (GTK does that anyway; the flag is tolerated for scripts);
 *   - closing the last window exits with status 0.
 *
 * Build: see build-fixtures.sh (gcc + pkg-config gtk+-3.0).
 */
#include <gtk/gtk.h>
#include <stdio.h>
#include <string.h>

static const char *base_title = "RAST Test App";
static int window_count = 0;
static GtkWidget *entry = NULL;
static GtkWidget *mirror = NULL;

static void spawn_window(void);

static void on_entry_changed(GtkEditable *editable, gpointer user_data) {
    (void)editable;
    (void)user_data;
    const char *text = gtk_entry_get_text(GTK_ENTRY(entry));
    char buffer[512];
    snprintf(buffer, sizeof(buffer), "You typed: %s", text);
    gtk_label_set_text(GTK_LABEL(mirror), buffer);
}

static void on_new_window(GtkButton *button, gpointer user_data) {
    (void)button;
    (void)user_data;
    spawn_window();
}

static void on_destroy(GtkWidget *widget, gpointer user_data) {
    (void)widget;
    (void)user_data;
    window_count--;
    if (window_count <= 0) {
        gtk_main_quit();
    }
}

static void spawn_window(void) {
    window_count++;
    char title[256];
    if (window_count == 1) {
        snprintf(title, sizeof(title), "%s", base_title);
    } else {
        snprintf(title, sizeof(title), "%s #%d", base_title, window_count);
    }

    GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_title(GTK_WINDOW(window), title);
    gtk_window_set_default_size(GTK_WINDOW(window), 720, 480);
    gtk_window_set_resizable(GTK_WINDOW(window), TRUE);
    g_signal_connect(window, "destroy", G_CALLBACK(on_destroy), NULL);

    GtkWidget *grid = gtk_grid_new();
    gtk_grid_set_row_spacing(GTK_GRID(grid), 12);
    gtk_grid_set_column_spacing(GTK_GRID(grid), 12);
    gtk_container_set_border_width(GTK_CONTAINER(grid), 18);
    gtk_container_add(GTK_CONTAINER(window), grid);

    GtkWidget *heading = gtk_label_new(NULL);
    gtk_label_set_markup(GTK_LABEL(heading),
                         "<span size='x-large' weight='bold'>RAST Test App</span>");
    gtk_widget_set_halign(heading, GTK_ALIGN_START);
    gtk_grid_attach(GTK_GRID(grid), heading, 0, 0, 2, 1);

    GtkWidget *hint = gtk_label_new("Deterministic GTK3 fixture for RAST end-to-end tests.");
    gtk_widget_set_halign(hint, GTK_ALIGN_START);
    gtk_grid_attach(GTK_GRID(grid), hint, 0, 1, 2, 1);

    GtkWidget *new_button = gtk_button_new_with_label("New Window");
    gtk_widget_set_size_request(new_button, 160, 44);
    gtk_widget_set_halign(new_button, GTK_ALIGN_START);
    g_signal_connect(new_button, "clicked", G_CALLBACK(on_new_window), NULL);
    gtk_grid_attach(GTK_GRID(grid), new_button, 0, 2, 2, 1);

    entry = gtk_entry_new();
    gtk_entry_set_placeholder_text(GTK_ENTRY(entry), "Type here — the label mirrors it");
    gtk_widget_set_size_request(entry, 640, 44);
    g_signal_connect(entry, "changed", G_CALLBACK(on_entry_changed), NULL);
    gtk_grid_attach(GTK_GRID(grid), entry, 0, 3, 2, 1);

    mirror = gtk_label_new("You typed: ");
    gtk_widget_set_halign(mirror, GTK_ALIGN_START);
    gtk_label_set_ellipsize(GTK_LABEL(mirror), PANGO_ELLIPSIZE_END);
    gtk_widget_set_size_request(mirror, 640, -1);
    gtk_grid_attach(GTK_GRID(grid), mirror, 0, 4, 2, 1);

    gtk_widget_show_all(window);
    gtk_widget_grab_focus(entry);
}

int main(int argc, char **argv) {
    int initial_windows = 1;
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--windows") == 0 && i + 1 < argc) {
            initial_windows = atoi(argv[++i]);
            if (initial_windows < 1) initial_windows = 1;
            if (initial_windows > 8) initial_windows = 8;
        } else if (strcmp(argv[i], "--title") == 0 && i + 1 < argc) {
            base_title = argv[++i];
        } else if (strcmp(argv[i], "--hold") == 0) {
            /* no-op: GTK keeps running while a window is mapped */
        }
    }

    gtk_init(&argc, &argv);
    /* One entry/mirror pair is shared by the fixture's windows; keep it simple:
     * only the first window gets the interactive widgets, extra windows show
     * a status label. */
    for (int i = 0; i < initial_windows; i++) {
        spawn_window();
        if (i == 0) {
            /* widgets are created inside spawn_window */
        }
    }
    gtk_main();
    return 0;
}
