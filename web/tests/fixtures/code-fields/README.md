# Historical Technical Views data

`previous-technical-views.xml` is the ERPForAll base addon data read with:

```
git show ce72f8f4ffbdc57f6e25f950909de2515d9bc2c9:addons/base/data/technical_views.xml
```

The browser upgrade rehearsal stores its actual `technical_view_form` architecture through authenticated ORM RPC, stops the server, then runs the normal base module upgrade and checks the new served code/XML declaration. It tests replacement of an existing data declaration; it does not claim execution of a previous release binary or an SQL schema migration.
