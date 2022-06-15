mod from_bytes {
    use git::prelude::ObjectIdExt;
    use git::RevSpec;
    use git_ref::bstr::{BString, ByteSlice};
    use git_repository as git;
    use git_testtools::hex_to_id;
    use once_cell::sync::Lazy;
    use std::collections::HashMap;
    use std::str::FromStr;

    const FIXTURE_NAME: &str = "make_rev_spec_parse_repos.sh";
    static BASELINE: Lazy<HashMap<BString, Option<git::ObjectId>>> = Lazy::new(|| {
        let mut map = HashMap::new();
        let base = git_testtools::scripted_fixture_repo_read_only(FIXTURE_NAME).unwrap();
        let baseline = std::fs::read(base.join("baseline.git")).unwrap();
        let mut lines = baseline.lines();
        while let Some(spec) = lines.next() {
            let exit_code_or_hash = lines.next().expect("exit code or single hash").to_str().unwrap();
            let possibly_hash = match u8::from_str(exit_code_or_hash) {
                Ok(_) => None,
                Err(_) => Some(git::ObjectId::from_str(exit_code_or_hash).unwrap()),
            };
            assert_eq!(
                map.insert(spec.into(), possibly_hash),
                None,
                "Duplicate spec '{}' cannot be handled",
                spec.as_bstr()
            );
        }
        map
    });

    fn parse_spec<'a>(spec: &str, repo: &'a git::Repository) -> Result<RevSpec<'a>, git::rev_spec::parse::Error> {
        let res = RevSpec::from_bstr(spec, repo);
        let actual = res.as_ref().ok().and_then(|rs| rs.from().map(|id| id.detach()));
        let spec: BString = spec.into();
        assert_eq!(
            &actual,
            BASELINE
                .get(&spec)
                .unwrap_or_else(|| panic!("'{}' revspec not found in git baseline", spec)),
            "{}: git baseline boiled down to success or failure must match our outcome",
            spec
        );
        res
    }

    fn repo(name: &str) -> crate::Result<git::Repository> {
        let base = git_testtools::scripted_fixture_repo_read_only(FIXTURE_NAME)?;
        Ok(git::open(base.join(name))?)
    }

    mod ambiguous {
        use super::repo;
        use crate::rev_spec::from_bytes::parse_spec;

        #[test]
        fn prefix() {
            {
                let repo = repo("blob.prefix").unwrap();
                assert_eq!(
                    parse_spec("dead", &repo).unwrap_err().to_string(),
                    "Found more than one object prefixed with dead\nThe ref partially named 'dead' could not be found"
                );
                assert_eq!(
                    parse_spec("beef", &repo).unwrap_err().to_string(),
                    "Found more than one object prefixed with beef\nThe ref partially named 'beef' could not be found"
                );
            }

            {
                let repo = repo("blob.bad").unwrap();
                assert_eq!(
                    parse_spec("bad0", &repo).unwrap_err().to_string(),
                    "Found more than one object prefixed with bad0\nThe ref partially named 'bad0' could not be found",
                    "git is able to also detect that the object has an invalid type, but we are not because the type doesn't matter here"
                );
            }
        }
    }

    #[test]
    fn bad_objects_are_valid_until_they_are_actually_read_from_the_odb() {
        let repo = repo("blob.bad").unwrap();
        let spec = parse_spec("e328", &repo).unwrap();
        assert_eq!(
            spec,
            RevSpec::from_id(hex_to_id("e32851d29feb48953c6f40b2e06d630a3c49608a").attach(&repo)),
            "we are able to return objects even though they are 'bad' when trying to decode them, like git",
        );
        assert_eq!(
            format!("{:?}", parse_spec("e328^{object}", &repo).unwrap_err()),
            r#"FindObject(Find(Loose(Decode(ObjectHeader(InvalidObjectKind("bad"))))))"#,
            "Now we enforce the object to exist and be valid, as ultimately it wants to match with a certain type"
        );
    }

    #[test]
    #[ignore]
    fn find_ref() {}
}
