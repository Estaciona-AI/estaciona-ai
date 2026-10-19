-- Serialize with registration; never discard an existing ownership record.
LOCK TABLE public.installation_binding IN ACCESS EXCLUSIVE MODE;
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM public.installation_binding) THEN
        RAISE EXCEPTION 'Cannot remove installation binding while a database is bound';
    END IF;
END $$;
DROP TABLE public.installation_binding;
