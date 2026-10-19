DO $$ BEGIN
    RAISE EXCEPTION 'R12 downgrade requires manual reconciliation; implicit plate ownership must not be restored';
END $$;
