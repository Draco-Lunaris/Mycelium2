-- Session ids are stored SHA-256 hashed as of this release. Legacy
-- plaintext-id session rows cannot resolve against hashed lookups; clear
-- them so no usable cookie value lingers at rest. Users re-login.
DELETE FROM sessions;