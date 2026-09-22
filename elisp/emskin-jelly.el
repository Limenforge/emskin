;;; emskin-jelly.el --- Jelly text-cursor animation  -*- lexical-binding: t; -*-

;; Algorithm ported from holo-layer's `holo-layer-get-cursor-info' — pure
;; elisp + IPC, no Wayland `text_input_v3' dependency (pgtk doesn't
;; broadcast caret position on that channel reliably when an IM like
;; fcitx intercepts the GTK IM context).

;;; Code:

(require 'emskin-ipc)
(require 'emskin-app)  ; emskin--frame-header-offset

(defvar emskin-jelly-cursor)
(defvar emskin-jelly-fallback-color)

(defvar emskin--jelly-last-info nil
  "Last sent jelly caret `(WINDOW . \"x:y:w:h:color\")' pair.
The window component is a cheap tiebreaker so moving to a different
window at the same pixel coords still fires a new animation.")

(defvar emskin--jelly-native-cursor-types
  (make-hash-table :test 'eq :weakness 'key)
  "Saved native `cursor-type' values, keyed by live buffer.")

(defvar-local emskin--jelly-cursor-prepared nil
  "Non-nil when `pre-command-hook' exposed this buffer's native cursor type.")

(defconst emskin--jelly-missing (make-symbol "emskin-jelly-missing"))

;; ---------------------------------------------------------------------------
;; Native cursor ownership
;; ---------------------------------------------------------------------------

(defun emskin--jelly-prepare-native-cursor ()
  "Restore the logical cursor type before a command can inspect or change it.
The matching post-command monitor captures the possibly changed type and hides
the native caret again before redisplay."
  (when (and emskin-jelly-cursor emskin--process)
    (let ((entry (gethash (current-buffer)
                          emskin--jelly-native-cursor-types
                          emskin--jelly-missing)))
      (unless (eq entry emskin--jelly-missing)
        (setq cursor-type (cdr entry)
              emskin--jelly-cursor-prepared t)))))

(defun emskin--jelly-hide-buffer-cursor (buffer &optional capture)
  "Hide BUFFER's native caret and return its logical cursor type.
When CAPTURE is non-nil, record a type exposed by
`emskin--jelly-prepare-native-cursor', including an intentional nil value."
  (with-current-buffer buffer
    (let ((entry (gethash buffer emskin--jelly-native-cursor-types
                          emskin--jelly-missing)))
      (cond
       ((eq entry emskin--jelly-missing)
        (setq entry (cons t cursor-type))
        (puthash buffer entry emskin--jelly-native-cursor-types))
       ((and capture emskin--jelly-cursor-prepared)
        (setcdr entry cursor-type))
       ((and capture cursor-type)
        ;; Also notice cursor changes made outside the command loop.
        (setcdr entry cursor-type)))
      (setq cursor-type nil
            emskin--jelly-cursor-prepared nil)
      (cdr entry))))

(defun emskin--jelly-hide-visible-native-cursors ()
  "Hide native carets in every currently visible Emacs buffer."
  (dolist (frame (frame-list))
    (dolist (window (window-list frame t))
      (emskin--jelly-hide-buffer-cursor (window-buffer window)))))

(defun emskin--jelly-restore-native-cursors ()
  "Restore every native cursor hidden by the jelly effect."
  (maphash
   (lambda (buffer entry)
     (when (buffer-live-p buffer)
       (with-current-buffer buffer
         (setq cursor-type (cdr entry)
               emskin--jelly-cursor-prepared nil))))
   emskin--jelly-native-cursor-types)
  (clrhash emskin--jelly-native-cursor-types))

;; ---------------------------------------------------------------------------
;; Caret rect computation
;; ---------------------------------------------------------------------------

(defun emskin--jelly-window-origin (window)
  "Return (X Y) of WINDOW's top-left in Emacs surface coordinates."
  (let* ((frame (window-frame window))
         (edges (window-pixel-edges window))
         (header (emskin--frame-header-offset frame))
         ;; Child frame offset relative to the root (parent) frame surface.
         (frame-x (or (frame-parameter frame 'left) 0))
         (frame-y (or (frame-parameter frame 'top) 0)))
    (list (+ (nth 0 edges) frame-x)
          (+ (nth 1 edges) header frame-y))))

(defun emskin--jelly-glyph-width (position window fallback)
  "Return the rendered glyph width at POSITION in WINDOW.
FALLBACK is used at line ends or when the display engine has no glyph there."
  (let* ((posn (posn-at-point position window))
         (size (and posn (posn-object-width-height posn)))
         (width (car-safe size)))
    (if (and (numberp width) (> width 0)) width fallback)))

(defun emskin--jelly-overlay-cursor-p (position)
  "Return non-nil if a before-string at POSITION places the display cursor."
  (catch 'cursor
    (dolist (overlay (overlays-in position position))
      (let ((string (overlay-get overlay 'before-string)))
        (when (and (stringp string)
                   (> (length string) 0)
                   (get-text-property 0 'cursor string))
          (throw 'cursor t))))))

(defun emskin--jelly-display-cursor-position (window)
  "Return the displayed cursor's (X Y) when an overlay relocates it.
Emacs reports the buffer position after a before-string containing completion
candidates, while the cursor text property keeps the caret before them."
  (redisplay)
  (let ((info (window-cursor-info window)))
    (when (and info
               (>= (aref info 1) 0)
               (>= (aref info 2) 0))
      (list (aref info 1) (aref info 2)))))

(defun emskin--jelly-cursor-rect (cursor-shape)
  "Return (X Y W H COLOR) of the text caret in surface pixels, or nil."
  (when-let* ((shape cursor-shape)
              (p (point))
              (window (selected-window))
              (vis (if (emskin--jelly-overlay-cursor-p p)
                       (emskin--jelly-display-cursor-position window)
                     (pos-visible-in-window-p p window t)))
              (alloc (emskin--jelly-window-origin window)))
    (let* ((wx (nth 0 alloc))
           (wy (nth 1 alloc))
           (fringe-l (or (car (window-fringes window)) 0))
           (margin-l (or (car (window-margins window)) 0))
           (cw (frame-char-width))
           (glyph-w (emskin--jelly-glyph-width p window cw))
           (line-h (line-pixel-height))
           (kind (if (consp shape) (car shape) shape))
           (amount (and (consp shape) (cdr shape)))
           (cursor-w (if (eq kind 'bar)
                         (if (integerp amount) amount 1)
                       glyph-w))
           (cursor-h (if (eq kind 'hbar)
                         (if (integerp amount) amount 1)
                       line-h))
           (x (+ (nth 0 vis) wx fringe-l (* margin-l cw)))
           (y (+ (nth 1 vis) wy
                 (if (eq kind 'hbar) (- line-h cursor-h) 0)))
           (color (or (face-background 'cursor nil t)
                      emskin-jelly-fallback-color)))
      (list x y cursor-w cursor-h color))))

;; ---------------------------------------------------------------------------
;; Post-command monitor
;; ---------------------------------------------------------------------------

(defun emskin--jelly-send (info)
  "Send caret INFO (x y w h color) or nil (cancel) to the compositor."
  (emskin--send `((type . "set_cursor_rect")
                  (x . ,(or (nth 0 info) 0))
                  (y . ,(or (nth 1 info) 0))
                  (w . ,(or (nth 2 info) 0))
                  (h . ,(or (nth 3 info) 0))
                  (color . ,(or (nth 4 info) :null)))))

(defun emskin--jelly-push-current (&optional force)
  "Hide the native caret and push its synthetic replacement.
When FORCE is non-nil, bypass the last-rectangle deduplication guard."
  (let* ((cursor-shape
          (emskin--jelly-hide-buffer-cursor (current-buffer) t))
         (info (emskin--jelly-cursor-rect cursor-shape))
         (window (selected-window)))
    (emskin--jelly-hide-visible-native-cursors)
    (cond
     ((null info)
      (when (or force emskin--jelly-last-info)
        (emskin--jelly-send nil)
        (setq emskin--jelly-last-info nil)))
     (t
      (let ((key (cons window
                       (format "%d:%d:%d:%d:%s"
                               (nth 0 info) (nth 1 info)
                               (nth 2 info) (nth 3 info) (nth 4 info)))))
        (when (or force (not (equal key emskin--jelly-last-info)))
          (emskin--jelly-send info)
          (setq emskin--jelly-last-info key)))))))

(defun emskin--jelly-monitor ()
  "Push the current caret rect if it moved.
Self-gates on `emskin-jelly-cursor' and `emskin--process' so the hook
can stay permanently installed on `post-command-hook'."
  (when (and emskin-jelly-cursor emskin--process)
    (emskin--jelly-push-current)))

(defun emskin--jelly-focus-out ()
  "Hide the synthetic caret while the Emacs frame lacks focus."
  (when (and emskin-jelly-cursor emskin--process)
    (emskin--jelly-send nil)
    (setq emskin--jelly-last-info nil)))

(defun emskin--jelly-focus-in ()
  "Re-prime the synthetic caret when the Emacs frame regains focus."
  (when (and emskin-jelly-cursor emskin--process)
    (emskin--jelly-push-current t)))

(defun emskin--jelly-cursor-sync ()
  (if emskin-jelly-cursor
      (progn
        ;; Enable the renderer before giving it the initial target.
        (emskin--send '((type . "set_jelly_cursor") (enabled . t)))
        (emskin--jelly-push-current t))
    (emskin--jelly-restore-native-cursors)
    (setq emskin--jelly-last-info nil)
    (emskin--send '((type . "set_jelly_cursor")
                    (enabled . :json-false)))))

(emskin-define-toggle jelly-cursor "jelly cursor")

(add-hook 'pre-command-hook #'emskin--jelly-prepare-native-cursor)
(add-hook 'post-command-hook #'emskin--jelly-monitor)
(add-hook 'focus-out-hook #'emskin--jelly-focus-out)
(add-hook 'focus-in-hook #'emskin--jelly-focus-in)
(add-hook 'emskin-disconnected-hook #'emskin--jelly-restore-native-cursors)

(provide 'emskin-jelly)
;;; emskin-jelly.el ends here
